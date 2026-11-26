# Ghost Browser — Arquitectura

Visión general del navegador de escritorio **Ghost**, escrito en Rust, con dos motores de renderizado:

- **WebView2 (Chromium)** — motor completo, compatible con cualquier sitio.
- **Wisp** — motor HTML/CSS/JS ligero y propio, para páginas estáticas y documentación.

La aplicación usa `egui` + `eframe` para la UI nativa, `wry` como puente a WebView2 y un IPC basado en `postMessage` para comunicar la página web con el proceso Rust.

---

## 1. Estructura de alto nivel

```
src/
├── main.rs          # Punto de entrada, bucle eframe, integración nativa
├── browser.rs       # Lógica central: pestañas, motores, historial, sesiones, red
├── ui.rs            # UI con egui: chrome, pestañas, barra de direcciones, find-bar, paneles
├── webview.rs       # WebView2 + página "newtab" + IPC (canal window.ipc)
├── store.rs         # Persistencia: settings, bookmarks, history, sessions
├── remote.rs        # API HTTP local (127.0.0.1:47820) para automatización / MCP
├── perf.rs          # Trazas de rendimiento (opcional, feature "perf")
├── logo.rs          # Generación del icono / logo vectorial
├── net.rs           # Utilidades de red (detección offline / slow)
```

El motor Wisp y su capa de agentes viven en el workspace `wisp/`, con crates
separados: `wisp-cdp` (subconjunto CDP + captura PNG), `wisp-mcp` (puente MCP
stdio, agnóstico de host), `wisp-agent` (binario headless, sin UI), `wisp-idl`
(descriptores de interfaz generados desde fragmentos WebIDL) y `wisp-js` /
`wisp-paint` (JS y pintado propios).

---

## 2. Hilos y concurrencia

| Hilo / Componente          | Responsabilidad                                                                 |
|----------------------------|----------------------------------------------------------------------------------|
| **UI thread (eframe)**     | Dibuja la UI, procesa eventos de teclado/ratón, llama a `Browser::update()`      |
| **WebView2 (wry)**         | Un hilo interno de wry/Win32 por cada `WebView`; callbacks de IPC llegan aquí    |
| **Rayon thread-pool**      | Cargas Wisp (`WispLoad`), layouts Wisp (`WispLayout`) — **N hilos fijos**       |
| **Remote API**             | `TcpListener` con un hilo por conexión en `remote.rs` (puerto 47820, configurable) |
| **MCP (opcional)**         | crate `wisp-mcp` (stdio JSON-RPC 2.0, sin `tokio`)                               |

**Regla de oro:** la UI **nunca** bloquea. Operaciones costosas (red, parseo HTML, layout Wisp) se despachan al pool de Rayon o a hilos dedicados; los resultados vuelven por canales `mpsc` y se procesan en `Browser::update()` (UI thread).

---

## 3. Modelo de pestañas (`Tab`)

```rust
pub struct Tab {
    pub id: u64,
    pub url: String,
    pub title: String,
    pub engine: BrowserEngine,          // WebView2 | Wisp
    pub webview: Option<WebView>,       // solo WebView2
    pub wisp_doc: Option<WispDocument>, // solo Wisp
    pub navigation_history: Vec<String>,
    pub history_index: usize,
    pub suspended: bool,                // true → no consume WebView2
    pub loading: bool,
    pub favicon: Option<egui::TextureHandle>,
    // … estado de find-in-page, zoom, user-scripts, etc.
}
```

- **Suspend/resume:** al cambiar de pestaña, la anterior se *suspende* (se destruye su `WebView` si usa Chromium) y la nueva se *resume* (se crea `WebView` si hacía falta). Esto ahorra memoria.
- **Motor por pestaña:** cada pestaña recuerda su motor (`WebView2` / `Wisp` / `Auto`). `Auto` deja que la heurística (`wisp_needs_webview`) decida.

---

## 4. Motor Wisp (HTML/CSS/JS ligero)

### 4.1 Pipeline

```
HTML (string) ──► html5ever (parser) ──► DOM tree
CSS (string)  ──► cssparser / selectors ──► Styled nodes
Layout        ──► taffy (flexbox/grid)  ──► Layout tree (LayoutRect)
Paint         ──► egui primitives       ──► Textures / Meshes
JS            ──► boa_engine            ──► Event loop + DOM mutations
```

### 4.2 Hilos Wisp

- **`WispLoad`** (Rayon): parse HTML + CSS → `WispDocument` (DOM + estilos). No hace layout.
- **`WispLayout`** (Rayon): recibe `WispDocument` + ancho disponible → calcula layout → devuelve `WispLayoutResult` (cajas posicionadas, altura total, imágenes).
- **UI thread**: pinta el `WispLayoutResult` con egui (texto, fondos, bordes, imágenes como texturas).

### 4.3 Texturas Wisp (LRU)

`App.wisp_textures: HashMap<String, egui::TextureHandle>` + `VecDeque<String>` (orden LRU). Límite configurable `max_wisp_textures` (default 200).

---

## 5. IPC — `window.ipc.postMessage`

La página web (newtab o cualquier sitio) se comunica con Rust vía:

```js
window.ipc.postMessage(JSON.stringify({ type: "navigate", url: "https://example.com" }));
```

Rust escucha en `WebView::on_webview_message` → `TabEvent::IpcMessage` → `Browser::handle_ipc`.

**Tipos principales:**

| type (JS → Rust)        | Payload                          | Acción en Rust                        |
|-------------------------|----------------------------------|---------------------------------------|
| `navigate`              | `{url}`                          | `load_url(tab_id, url)`               |
| `new_tab`               | `{url?, engine?}`                | `new_tab(...)`                        |
| `close_tab`             | `{tab_id}`                       | `close_tab(tab_id)`                   |
| `switch_tab`            | `{tab_id}`                       | `switch_tab(tab_id)`                  |
| `set_engine`            | `{tab_id, engine}`               | Cambia motor + recarga                |
| `bookmark_add`          | `{url, title}`                   | `store.add_bookmark`                  |
| `history_clear_range`   | `{from, to}`                     | Borra historial por fecha             |
| `settings_get`          | `{}`                             | Responde con `settings`               |
| `settings_set`          | `{key, value}`                   | Actualiza `Settings` + guarda         |
| `spa_probe`             | `{len, html}`                    | Heurística SPA vacía → recarga fuerza |

**Rust → JS:** `webview.evaluate_script("window.ipc.onMessage(...)")` (eventos `tab_created`, `tab_updated`, `navigation`, `find_result`, `spa_probe`, etc.)

---

## 6. Persistencia (`Store`)

`Store` se serializa a `ghost_store.json` (pretty JSON) en la carpeta de datos de la app (`dirs::data_local_dir()/Ghost`).

**Campos clave:**

```rust
pub struct Settings {
    pub default_engine: BrowserEngine,        // WebView2 | Wisp | Auto
    pub search_engine: String,                // "duckduckgo", "google", …
    pub home_url: String,                     // "ghost:newtab" o URL
    pub privacy_block_third_party: bool,
    pub privacy_send_do_not_track: bool,
    pub max_history_per_tab: usize,           // 100
    pub max_restored_tabs: usize,             // 50
    pub restore_timeout_secs: u64,            // 30
    pub max_pending_history_index: usize,     // 1000
    pub max_pending_wisp_requests: usize,     // 50
    pub max_pending_wisp_layouts: usize,      // 50
    pub max_spa_probe_at: usize,              // 100
    pub spa_probe_delay_secs: u64,            // 3
    pub max_restore_started: usize,           // 100
    pub max_pending_remote_evals: usize,      // 200
    pub max_wisp_textures: usize,             // 200
    pub max_remembered_sites: usize,          // 200
}
```

- **Autosave:** `mark_dirty()` + timer 2 s → `save_if_dirty()`.
- **Migración:** `load()` acepta BOM UTF-8 y esquemas antiguos (campos opcionales con `Default`).

---

## 7. Restauración de sesión (arranque)

1. `Store::load()` → `SessionTab` (url, engine, history, index, active).
2. `Browser::restore_session()` crea pestañas **suspendidas** (solo metadata).
3. La pestaña activa se **resume** primero (crea WebView2 si hace falta).
4. Las demás se restauran **bajo demanda** al hacer clic (cola `pending_restore`).
5. **Timeout de seguridad:** `restore_timeout_secs` (default 30 s) → si una restauración no termina, se libera el overlay y se marca como fallida; el usuario puede reintentar.

---

## 8. API Remota / MCP

- **HTTP API** (`remote.rs`): rutas `health`, `tabs` (GET/POST), `navigate`, back/forward/reload/close/activate, `text`, `eval`, `click`, `type`, `press`, `scroll`, `move`, `cdp` y `screenshot`.
- **MCP** (crate `wisp-mcp`, binarios `wisp-mcp` y el alias histórico `ghost-mcp`): expone `tools/list`, `tools/call` vía stdio JSON-RPC 2.0. Herramientas: `ghost_health`, `ghost_tabs`, `ghost_open_tab`, `ghost_navigate`, `ghost_tab_action`, `ghost_eval`, `ghost_click`, `ghost_type`, `ghost_press`, `ghost_scroll`, `ghost_move`, `ghost_screenshot` y `ghost_cdp`.

Ambas hablan el contrato compartido `wisp_mcp::control`: el mismo cliente MCP funciona contra Ghost y contra `wisp-agent serve` sin cambiar configuración. En Ghost, `eval`, `cdp` y `screenshot` se resuelven con WebView2 (`oneshot` / IPC).

---

## 9. Constantes de UI (chrome)

```rust
pub const UI_HEIGHT: f32 = 80.0;        // banda superior total
pub const CHROME_TOP_INSET: f32 = 6.0;  // banda de redimensionado (dragging)
pub const STATUS_HEIGHT: f32 = 24.0;    // barra de estado inferior
pub const FIND_BAR_HEIGHT: f32 = 34.0;  // barra "Buscar en la página"
```

El viewport web empieza en `y = UI_HEIGHT` y termina en `height - STATUS_HEIGHT - (find_active ? FIND_BAR_HEIGHT : 0)`.

---

## 10. Flujo de navegación (resumen)

```
Usuario escribe en Omnibox + Enter
        │
        ▼
normalize_url()  (añade esquema, detecta motor de búsqueda)
        │
        ▼
load_url(tab_id, url)
        │
        ├─► engine = Wisp ──► spawn WispLoad (Rayon) ──► WispDocument
        │                          │
        │                          └─► spawn WispLayout (Rayon) ──► WispLayoutResult ──► paint
        │
        └─► engine = WebView2 ──► ensure_webview() ──► webview.navigate(url)
                                   │
                                   └─► IPC PageStarted / PageFinished ──► record_navigation()
```

---

## 11. Tests

- **Unit/integration** en `#[cfg(test)]` dentro de cada módulo (`browser`, `store`, `ui`, `webview`).
- 49 tests cubren: historial, sesiones, suspensión, find-in-page, SPA probe, ad-blocker, normalización URL, UI dimensions, etc.
- Ejecutar: `cargo test --locked`

---

## 12. CI / CD

GitHub Actions (`.github/workflows/ci.yml`):

- **Windows:** build release, fmt, clippy `-D warnings`, tests.
- **Linux (Ubuntu):** mismo pipeline + dependencias GTK/WebKit para compilación cruzada futura.

---

## 13. Extensibilidad / Puntos de entrada

| Área                    | Dónde tocar                                    |
|-------------------------|------------------------------------------------|
| Nuevo motor de búsqueda | `store.rs` → `SearchEngine` + `normalize_url` |
| Nueva acción IPC        | `webview.rs` → `TabEvent::IpcMessage` + `browser.rs` → `handle_ipc` |
| Nuevo setting           | `store.rs` → `Settings` + UI en `ui.rs` (SettingsPanel) |
| Nueva herramienta MCP   | `wisp/crates/wisp-mcp/src/lib.rs` → `handle_request` |
| Nueva interfaz/miembro del DOM | `wisp/crates/wisp-idl/webidl/*.webidl` (descriptor) + `wisp/crates/wisp-js/src/dom.js` (implementación); el chequeo `wisp_js::idl::idl_conformance()` lista lo que falta |
| Nuevo test              | Módulo correspondiente → `#[test]`            |

---

## 14. Decisiones de diseño clave

1. **Una sola ventana, pestañas gestionadas por Rust** — no delegamos tabs a Chromium; control total del ciclo de vida.
2. **Suspend/resume agresivo** — solo la pestaña activa tiene `WebView` vivo (si usa Chromium).
3. **Wisp off-main-thread** — parseo y layout en Rayon; UI thread solo pinta.
4. **Settings como única fuente de límites** — nada de `const` mágicos dispersos; todo configurable y persistente.
5. **IPC basado en JSON** — simple, depurable, funciona igual en WebView2 y en la página `ghost:newtab`.
6. **Sin `unsafe` en código de aplicación** — solo en FFI de `wry`/`webview2` (encapsulado).

---

## 15. Roadmap técnico (ideas)

- [ ] Migrar `HashMap<u64, Tab>` → `Vec<Tab>` + `Generation` indices (evitar colisiones de ID).
- [ ] Añadir `feature = "perf"` guards en `perf.rs` para trazado en release.
- [ ] Soporte Wayland (Linux) vía `wry` + `gtk4`.
- [ ] Perfil de memoria: telemetría opcional de `WebView2` vs `Wisp`.
- [ ] Plugin system (WASM) para user-scripts / content-scripts.

---