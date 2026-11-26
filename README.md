<div align="center">
  <img src="ghost-mark.png" alt="Logotipo de Ghost" width="128">
  <h1>Ghost Browser</h1>
  <p>Un navegador de escritorio ligero en Rust con motor propio experimental (<strong>Wisp</strong>) y WebView2 opcional, más binario headless y servidor MCP para agentes.</p>
  <p>
    <a href="https://github.com/sarch-codelab/ghost-browser/releases/latest"><img src="https://img.shields.io/github/v/release/sarch-codelab/ghost-browser" alt="Última release"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/licencia-MIT-blue" alt="Licencia MIT"></a>
    <a href="https://github.com/sarch-codelab/ghost-browser/actions/workflows/ci.yml"><img src="https://github.com/sarch-codelab/ghost-browser/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
    <a href="https://github.com/sarch-codelab/ghost-browser/actions/workflows/release.yml"><img src="https://github.com/sarch-codelab/ghost-browser/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  </p>
</div>

Ghost combina una interfaz propia con dos motores: **Wisp**, un motor web
experimental escrito en Rust para páginas documentales, y **WebView2**
(Chromium) para aplicaciones web complejas. La página de nueva pestaña es
nativa y no crea procesos de Chromium: WebView2 solo arranca cuando una página
real lo necesita.

## Funciones

- **Pestañas:** abrir, cerrar, cambiar entre pestañas y reabrir las cerradas
  recientemente.
- **Restauración bajo demanda:** las pestañas guardadas se mantienen suspendidas
  al iniciar y se cargan cuando se seleccionan.
- **Ahorro de memoria:** suspender pestañas inactivas, ajustar el tiempo de
  inactividad y limitar cuántos webviews permanecen activos.
- **Motor híbrido:** Wisp (Rust) para artículos y documentación, WebView2
  (Chromium) para webapps, con conmutación manual por pestaña o automática
  por URL. YouTube y otras webapps de vídeo van siempre a WebView2, sin
  descargar antes la página con Wisp.
- **Avisos de red:** la barra de estado indica «Sin conexión a internet» o
  «Conexión lenta», y deja de recargar esqueletos SPA cuando la red va mal.
- **Ventanas adaptables:** la ventana principal y las subventanas (ajustes,
  marcadores, historial, lectura) se ajustan al área de trabajo del monitor
  actual, incluidos monitores externos más pequeños o con escalado distinto.
- **Nueva pestaña nativa:** la página de inicio se pinta con la propia
  interfaz sin arrancar Chromium.
- **Privacidad:** filtrado opcional de dominios conocidos en navegación y
  solicitudes `fetch`/XHR, pausa de medios en segundo plano y opción para borrar
  datos de navegación. No es un bloqueador exhaustivo.
- **Buscar en la página:** `Ctrl+F` con recuento de coincidencias, todas
  resaltadas, y salto entre ellas (anterior/siguiente) sin ocultar el contenido.
  También en Wisp, donde el salto desplaza la vista hasta la coincidencia actual.
- **Marcadores e historial:** consultar y administrar ambos desde la interfaz,
  con borrado por período (últimas 24 h, últimos 7 días o todo).
- **Descargas:** elegir la carpeta de destino; Ghost añade sufijos a nombres
  repetidos cuando hay un nombre disponible.
- **Apariencia:** temas claro y oscuro, además de una página integrada para
  nuevas pestañas.
- **Herramientas:** página de inicio configurable, motor de búsqueda elegible
  (DuckDuckGo, Google, Bing, Brave Search, Startpage o Ecosia), zoom y
  herramientas de desarrollo.
- **Datos:** desde Ajustes se borra la caché del navegador completo o las
  cookies y todos los datos del sitio de la pestaña activa.

## Requisitos

Ghost está orientado actualmente a Windows y utiliza el runtime de Microsoft
Edge WebView2. Para compilar, instala:

1. Rust estable reciente y el target `x86_64-pc-windows-msvc`.
2. Visual Studio Build Tools con la carga de trabajo **Desarrollo para el
   escritorio con C++** y un Windows SDK. La compilación usa `rc.exe` para
   incluir el icono de la aplicación.
3. Microsoft Edge WebView2 Runtime.

## Compilar y ejecutar

Desde la carpeta del proyecto:

```powershell
cargo run --release
```

Para compilar sin iniciar la aplicación:

```powershell
cargo build --release
```

Para habilitar el trazado de rendimiento (memoria, tiempos de arranque, restauración):

```powershell
cargo run --release --features perf
```

Si no se encuentra `rc.exe`, instala el Windows SDK o define la variable de
entorno `RC` con la ruta completa al compilador de recursos.

## Atajos de teclado

| Atajo | Acción |
| --- | --- |
| `Ctrl+T` | Abrir una pestaña |
| `Ctrl+W` | Cerrar la pestaña actual |
| `Ctrl+Shift+T` | Reabrir la última pestaña cerrada |
| `Ctrl+L` | Enfocar la barra de direcciones |
| `Ctrl+F` | Buscar en la página |
| `Ctrl+R` o `F5` | Recargar la página |
| `Alt+Izquierda` / `Alt+Derecha` | Atrás / adelante |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Pestaña siguiente / anterior |
| `Ctrl+D` | Añadir o quitar un marcador |
| `Ctrl+B` | Abrir marcadores |
| `Ctrl+H` | Abrir historial |
| `Ctrl++` / `Ctrl+-` / `Ctrl+0` | Aumentar / reducir / restablecer zoom |
| `Ctrl+P` | Imprimir |
| `F12` | Abrir herramientas de desarrollo |
| `Ctrl+Shift+W` | Cambiar el motor de la pestaña (Wisp ↔ WebView2) |

## Variables de entorno de configuración

Todos los ajustes persistidos en `ghost.json` se pueden sobrescribir al arrancar
con variables de entorno con prefijo `GHOST_`. Ejemplos:

| Variable | Descripción | Valor por defecto |
| --- | --- | --- |
| `GHOST_DARK_MODE` | Tema oscuro | `true` |
| `GHOST_DEFAULT_ENGINE` | Motor por defecto: `webview2` o `wisp` | `webview2` |
| `GHOST_HOME_PAGE` | URL de la página de inicio | `ghost://newtab` |
| `GHOST_BLOCK_ADS` | Bloquear anuncios/rastreadores | `true` |
| `GHOST_SEARCH_ENGINE` | Buscador: `duckduckgo`, `google`, `bing`, `brave`, `startpage`, `ecosia` | `duckduckgo` |
| `GHOST_MAX_HISTORY_PER_TAB` | Entradas de historial por pestaña | `50` |
| `GHOST_MAX_RESTORED_TABS` | Pestañas visibles al iniciar | `200` |
| `GHOST_RESTORE_TIMEOUT_SECS` | Timeout de restauración (s) | `15` |
| `GHOST_MAX_WISP_TEXTURES` | Texturas Wisp en caché (LRU) | `256` |
| `GHOST_MAX_LIVE_TABS` | Webviews vivos simultáneos | `3` |
| `GHOST_SUSPEND_AFTER_SECS` | Segundos antes de suspender pestaña | `60` |
| `GHOST_MEMORY_SAVER` | Activar ahorro de memoria | `true` |
| `GHOST_AUTO_ENGINE` | Elegir motor automáticamente por URL | `false` |
| `GHOST_ENGINE` | Forzar motor al arrancar (`wisp` o `webview2`) | (según ajuste) |
| `GHOST_DOWNLOAD_DIR` | Carpeta de descargas | (sistema) |
| `GHOST_TIMING_LOG` | Archivo de log de rendimiento/timing | (desactivado) |
| `WISP_REMOTE_PORT` / `GHOST_REMOTE_PORT` | Puerto del API de control local | `47820` |
| `WISP_ALLOW_PRIVATE_NETWORK` | Permitir IPs privadas/localhost en Wisp (`1`/`true`) | `false` |
| `WISP_SCRIPT_DEADLINE_MS` / `WISP_MODULE_BUDGET_MS` / `WISP_FETCH_DEADLINE_MS` | Presupuestos duros de red y JS | `120` / `500` / `8000` |

Todas las opciones están definidas en `src/store.rs` (`Settings::from_env()`).

## Control programático y agentes de IA

Ghost expone una **API HTTP local** para que otras herramientas y agentes de
IA conduzcan el navegador. Escucha solo en `127.0.0.1` (ningún equipo externo
puede conectarse) en el puerto `47820`, configurable con la variable de
entorno `GHOST_REMOTE_PORT`.

| Método y ruta | Acción |
| --- | --- |
| `GET /health` | Comprueba que la API responde |
| `GET /tabs` (`?engine=wisp`) | Lista pestañas: id, título, url, motor, estado (`?engine=wisp` filtra solo pestañas Wisp) |
| `POST /tabs` | `{"url": "..."}` abre una pestaña nueva |
| `POST /tabs/{id\|active}/navigate` | `{"url": "..."}` navega la pestaña (o la activa con `active`) |
| `POST /tabs/{id\|active}/back` `/forward` `/reload` `/close` `/activate` | Acciones de historial y gestión |
| `POST /tabs/{id\|active}/eval` | `{"js": "..."}` ejecuta JS (WebView2 por IPC o Wisp vía `eval_js`) y devuelve el resultado (`400` si el JSON está malformado) |
| `POST /tabs/{id\|active}/text` | Texto legible de la página (reader en Wisp, `innerText` en WebView2) |
| `POST /tabs/{id\|active}/snapshot` | `{"width"?}` snapshot accesible con referencias estables `@e1`, `@e2`…, rol y centralidad |
| `POST /tabs/{id\|active}/click` | `{"ref"?,"x"?,"y"?,"button"?,"clicks"?}` clic por `@ref` o coordenadas CSS |
| `POST /tabs/{id\|active}/fill` / `select_option` | `{"ref"?,"name"?,"value"}` rellena o selecciona un control del formulario |
| `POST /tabs/{id\|active}/type` | `{"ref"?,"text": "..."}` inserta texto en el foco o `@ref` |
| `POST /tabs/{id\|active}/press` | `{"key": "enter"}` pulsa una tecla (enter, tab, esc, flechas, etc.) |
| `POST /tabs/{id\|active}/scroll` | `{"deltaX","deltaY","x"?,"y"?}` rueda del ratón |
| `POST /tabs/{id\|active}/move` | `{"x","y"}` mueve el cursor o resuelve el enlace bajo `(x, y)` |
| `POST /tabs/{id\|active}/wait_for` | `{"text"?}` espera/comprueba la presencia de texto |
| `GET /tabs/{id\|active}/network` / `console` / `events` | Registro de red, mensajes de consola y eventos CDP |
| `POST /tabs/{id\|active}/cdp` | `{"method","params"}` método CDP (WebView2 nativo o dispatcher headless en `wisp-agent`, incl. `LP.getMarkdown`) |
| `POST /tabs/{id\|active}/screenshot` | `{"path"?}` captura PNG (WebView2 `CapturePreview` o rasterizador de `wisp-agent`) |
| `POST /tabs/{id\|active}/pdf` | `{"path"?}` exporta la página actual a PDF (`wisp-agent`) |

> Nota sobre sesiones restauradas e IDs: al arrancar Ghost, las pestañas restauradas quedan suspendidas (`suspended: true`) con IDs correlativos (`1`, `2`, ...) y con `--wisp` / `GHOST_ENGINE=wisp` pasan a Wisp salvo los sitios de `site_requires_chromium`. Para evitar depender de un ID numérico variable, usa `/tabs/active/...` o consulta primero `GET /tabs?engine=wisp`.

### `wisp-agent`: motor headless sin UI ni Chromium

El binario `wisp-agent` (dentro del workspace `wisp/`) ejecuta únicamente el motor Wisp sin `egui`, `wgpu`, `wry` ni WebView2, bloqueando IPs privadas/loopback por defecto (protección SSRF, desactivable con `--allow-private-network`):

```powershell
cargo run --manifest-path wisp/Cargo.toml --release -p wisp-agent -- fetch https://example.com --dump markdown
cargo run --manifest-path wisp/Cargo.toml --release -p wisp-agent -- screenshot https://example.com --out out.png
cargo run --manifest-path wisp/Cargo.toml --release -p wisp-agent -- serve --port 47820 --workers 4
```

Ejemplo contra la API HTTP (usando `active` y filtro `?engine=wisp`):

```powershell
curl "http://127.0.0.1:47820/tabs?engine=wisp"
curl -X POST http://127.0.0.1:47820/tabs/active/snapshot -d '{}'
curl -X POST http://127.0.0.1:47820/tabs/active/eval -d '{"js":"document.title"}'
curl -X POST http://127.0.0.1:47820/tabs/active/click -d '{"ref":"@e1"}'
curl -X POST http://127.0.0.1:47820/tabs/active/screenshot -d '{}'
```

### `wisp-mcp`: Ghost (y el motor headless) como herramienta MCP

El puente MCP vive en el crate `wisp-mcp`, con dos binarios: `wisp-mcp` y el
alias histórico `ghost-mcp`. Habla
[Model Context Protocol](https://modelcontextprotocol.io) por stdio
(JSON-RPC 2.0). Cualquier cliente MCP — Claude Desktop, Cursor, Devin,
scripts propios — puede usar el navegador como herramienta nativa sin tocar la
API HTTP directamente:

```powershell
cargo build --manifest-path wisp/Cargo.toml --release -p wisp-mcp
```

El **mismo binario y la misma configuración** controlan Ghost (WebView2/Wisp) o el
motor headless `wisp-agent serve`; solo cambia el puerto, que se fija con
`WISP_REMOTE_PORT` / `GHOST_REMOTE_PORT` (por defecto `127.0.0.1:47820`).

Configuración MCP de ejemplo:

```json
{
  "mcpServers": {
    "ghost": {
      "command": "C:/ruta/a/wisp-mcp.exe"
    }
  }
}
```

Herramientas expuestas:
- **Unificadas (`browser_*`)**: `browser_navigate`, `browser_snapshot` (`@e1`, `@e2`…), `browser_screenshot`, `browser_pdf`, `browser_click` (por `ref` o `x,y`), `browser_fill`, `browser_type`, `browser_press_key`, `browser_select_option`, `browser_evaluate`, `browser_wait_for`, `browser_network_requests`, `browser_console_messages`, `browser_close`.
- **Históricas (`ghost_*`)**: `ghost_health`, `ghost_tabs`, `ghost_open_tab`, `ghost_navigate`, `ghost_tab_action`, `ghost_eval`, `ghost_click`, `ghost_type`, `ghost_press`, `ghost_scroll`, `ghost_move`, `ghost_screenshot` y `ghost_cdp`.

> **Aviso de seguridad**: la API no tiene autenticación porque solo escucha
> en loopback, pero cualquier proceso local de tu equipo puede controlar el
> navegador mientras Ghost o `wisp-agent serve` estén abiertos. No la expongas a la red.

## Privacidad y datos

El bloqueador integrado utiliza una lista limitada de patrones de dominios
conocidos; no es un bloqueador de contenido exhaustivo ni hace anónima la
navegación. Los sitios que visitas siguen recibiendo solicitudes de tu
dispositivo.

El motor de búsqueda se elige en **Ajustes → Búsqueda** y se aplica tanto a la
barra de direcciones como a la página de inicio. Desde **Ajustes → Datos** se
borra la caché de todo el navegador, y las cookies o los datos por sitio.

Los marcadores, el historial, los ajustes y la información de sesión se guardan
localmente en el directorio de datos de Ghost. En Windows, normalmente se
encuentran en `%LOCALAPPDATA%\Ghost\ghost.json`. WebView2 también administra
datos de sitios, como cookies y caché; puedes borrarlos desde **Ajustes →
Borrar cookies y datos de navegación**.

## Desarrollo

Para comprobar el formato y ejecutar las pruebas:

```powershell
cargo fmt --all -- --check
cargo check --all-targets
cargo test --locked
cargo test --manifest-path wisp/Cargo.toml --workspace --locked
```

Para ejecutar los benchmarks del motor (`wisp-benchmark`, tabla en [`WISP_RESULTS.md`](WISP_RESULTS.md)):

```bash
./scripts/run-all.sh
```

Para probar con la feature de rendimiento:

```powershell
cargo test --features perf
```

El código principal está en `src/` (`browser.rs`, `webview.rs`, `ui.rs`,
`remote.rs`) y el motor experimental en `wisp/`. `build.rs`, `ghost.rc` y
`ghost.ico` configuran el recurso de Windows.

Para contribuir, lee [CONTRIBUTING.md](CONTRIBUTING.md). Para reportar una
vulnerabilidad de seguridad, sigue [SECURITY.md](SECURITY.md) — no abras un
issue público. El historial de versiones está en [CHANGELOG.md](CHANGELOG.md).

## Rendimiento y memoria

Ghost implementa múltiples estrategias de economía de recursos para minimizar el uso de RAM:

- **Suspensión agresiva de pestañas**: Máximo 3 webviews vivos simultáneos, pestañas inactivas se suspenden tras 60s (configurable).
- **Motor Wisp (Rust puro)**: Renderiza sin procesos de Chromium — todo en un solo proceso.
- **Límites de recursos**: Historial por pestaña (50 URLs), texturas Wisp (256 con LRU), marcadores (200), historial global (1000).
- **Límites en HashMaps pendientes**: Operaciones pendientes limitadas a 20-50 entradas para prevenir memory leaks.
- **Página de inicio nativa**: Sin WebView2 hasta navegar a una URL real.
- **Restauración bajo demanda**: Solo 200 pestañas visibles al inicio, resto en disco.

### Benchmark

Medición reproducible con `ram_bench.ps1` (Windows, build `--release`):
Ghost con N pestañas de artículos de Wikipedia frente a Edge y a Ghost con
WebView2 forzado. Se mide el Working Set (WS) y los Private Bytes (PB) del
**árbol de procesos completo**, tras 35 s de carga.

| Pestañas | Ghost + Wisp (WS / PB) | Ghost + WebView2 (WS / PB) | Edge (WS / PB) |
| ---: | --- | --- | --- |
| 1 | 140.9 / 135.4 MB | 476.3 / 298.8 MB | 1131.2 / 649.4 MB |
| 5 | 141.7 / 143.6 MB | 763.0 / 480.3 MB | 1068.9 / 552.5 MB |
| 10 | 151.9 / 154.3 MB | 1100.3 / 704.9 MB | 1471.3 / 760.8 MB |
| 20 | 229.1 / 231.5 MB | 2641.0 / 1937.2 MB | 2398.8 / 1319.5 MB |

Con 20 pestañas, Ghost + Wisp usa **~10× menos memoria que Edge** y el árbol
completo es **un solo proceso** (`ghost.exe`): las páginas Wisp viven dentro
del proceso principal, sin renderizadores ni GPU de Chromium. El coste
marginal es de ~4–5 MB por pestaña. Los números de Edge incluyen su perfil
estándar; WebView2 dentro de Ghost se midió con las mismas URLs.

La comparación es deliberadamente conservadora: Wikipedia son páginas
textuales medias. En webs con mucho JavaScript o video, la diferencia crece
porque Chromium debe ejecutar todo ese trabajo mientras Wisp solo renderiza
lo que soporta.

### Monitoreo de memoria

Para monitorear el uso de memoria en tiempo de ejecución, compila con la feature `perf`
y establece la variable de entorno `GHOST_TIMING_LOG` con la ruta de un archivo de log:

```powershell
$env:GHOST_TIMING_LOG = "C:\temp\ghost_memory.log"
cargo run --release --features perf
```

Ghost reportará periódicamente el Working Set y Private Bytes cuando suspenda pestañas o guarde cambios,
además de hitos de arranque y restauración.

## Wisp experimental

Wisp está integrado como motor **Beta** seleccionable por pestaña; WebView2
sigue siendo el predeterminado para webapps. Wisp solicita páginas HTTP/HTTPS, hojas CSS,
imágenes y scripts dentro de límites, y aplica una cascada CSS con
especificidad real que incluye consultas `@media` según el ancho de la pestaña,
variables `var(--token)`, unidades `em`/`rem`/`%`, `calc()`, `min`/`max-width`,
márgenes `auto`, `line-height`, `white-space`, listas, bordes por lado y
flex/grid (con `minmax()`, `repeat(auto-fill|auto-fit, …)` y colocación
explícita por `grid-column`/`grid-row`), además de `min()`/`max()`/`clamp()`,
`aspect-ratio` y bloques `@supports`. Los scripts inline, externos y los
módulos ES se ejecutan sobre un DOM mutable con eventos, temporizadores,
`fetch`/XHR acotados (con CORS, presupuesto de red por página y avisos
cuando CORS o un límite bloquean la petición), Web Components básicos
(`customElements.define` y `connectedCallback`), `localStorage`, `location`
y `history` básicos; los fallos de JavaScript se muestran como advertencias
sin impedir el renderizado estático. La descarga de una página también tiene
un presupuesto global (8 s) para que una web pesada no bloquee la carga
encadenando esperas. No implementa todas las APIs de JavaScript o CSS
(formularios, shadow DOM completo, WebSocket, `position: fixed`), así que las
webs modernas e interactivas pueden verse incompletas: cuando Wisp detecta
que la página depende de esas APIs, Ghost la entrega automáticamente a
WebView2 en lugar de mostrar un esqueleto roto.

Opcionalmente, **Ajustes → "Elegir motor automáticamente por URL"** envía
artículos, documentación y wikis a Wisp y deja el resto en WebView2. El
**modo lectura** (botón de libro en la barra) extrae el texto limpio del
DOM de Wisp, sin anuncios ni scripts, listo para copiar o alimentar a una IA.

La meta a largo plazo es que Wisp ofrezca más estabilidad y rendimiento que
WebView2 en sus áreas compatibles; el benchmark anterior es la primera
medición reproducible y habrá que ampliarlo a más tipos de página.

El motor y el prototipo CLI están documentados en [`wisp/`](wisp/README.md).

## Licencia

Ghost Browser se distribuye bajo la licencia [MIT](LICENSE). Eres libre de
usarlo, modificarlo y redistribuirlo — incluso en productos comerciales —
conservando el aviso de copyright.
