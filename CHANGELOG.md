# Changelog

Todos los cambios notables de Ghost Browser se documentan aquí.
El formato sigue [Keep a Changelog](https://keepachangelog.com/es/1.1.0/) y
el proyecto usa [Versionado Semántico](https://semver.org/lang/es/).

## [Sin publicar]

### Añadido
- **Buscar en la página** (`Ctrl+F`): barra propia con recuento exacto,
  resaltado de todas las coincidencias y salto entre ellas (Enter,
  Shift+Enter o botones), sin ocultar el contenido. Distingue «Buscando…» de
  «sin coincidencias» cuando la página aún no respondió y repite la búsqueda
  al terminar de cargar. En Wisp cuenta sobre el texto maquetado, resalta todas
  las coincidencias, marca la actual y desplaza la vista hasta ella.
- **Motor de búsqueda configurable** (Ajustes → Búsqueda): DuckDuckGo, Google,
  Bing, Brave Search, Startpage o Ecosia. Lo usan la barra de direcciones y la
  página de inicio.
- **Borrado de datos** (Ajustes → Datos): la caché del navegador completo, y
  las cookies o todos los datos del sitio de la pestaña activa, vía Chrome
  DevTools Protocol.
- **Motor y agentes separados de la UI**: `src/cdp.rs` y `src/bin/ghost-mcp.rs`
  se mueven a los crates `wisp-cdp` y `wisp-mcp` del workspace `wisp/`, sobre un
  contrato de control compartido (`wisp_mcp::control`) y con el binario headless
  `wisp-agent` (`fetch` y `serve`). El mismo cliente MCP —y su configuración—
  controla Ghost y `wisp-agent` indistintamente.
- **Descriptores del DOM generados desde WebIDL**: el crate `wisp-idl` parsea
  fragmentos `.webidl` (`EventTarget`, `Node`, `Element`, `HTMLElement`,
  `Document` y `Window`) y genera, en tiempo de compilación, las tablas Rust de
  interfaces, atributos y operaciones que describen el DOM que Wisp expone a
  JavaScript.
- **Chequeo de conformidad del DOM**: `wisp-js` compara los descriptores WebIDL
  con el DOM real que instala `dom.js` (`wisp_js::idl::idl_conformance()`) y
  devuelve la lista exacta de interfaces y miembros que faltan, para alinear el
  runtime con el IDL sin revisar el código a mano. El DOM de `dom.js` ya cubre
  todos los miembros descritos: `Document` y `Window` instalados como
  constructores, `Node.nodeName`/`nodeValue`/`isEqualNode()`, los atributos de
  `HTMLElement` (`title`, `lang`, `dir`, `hidden`, `accessKey`, `outerText`) y
  `Document.doctype`/`readyState`/`images`/`links`.
- **Evaluación de JS en el motor Wisp**: `wisp-js` expone `evaluate_expression`
  y `wisp-engine` la publica como `FetchedPage::eval_js()`, y `wisp-agent serve`
  responde ya a `POST /tabs/{id}/eval` (`ghost_eval`). La evaluación levanta un
  contexto Boa efímero sobre el HTML de la página: ve el DOM y devuelve el valor
  serializado, más el HTML resultante cuando el script muta la página. Como el
  contexto es efímero, no conserva las variables que definió la página ni los
  cambios entre llamadas.
- **Entrada en `wisp-agent`**: `POST /tabs/{id}/click` resuelve el enlace bajo
  las coordenadas (`FetchedPage::hit_test`, sobre la lista de pintura: texto con
  `href` o imagen con `href`) y navega, y `POST /tabs/{id}/move` informa del
  enlace bajo el punto. La GUI usa el mismo criterio. Las operaciones que
  necesitan foco, desplazamiento o CDP nativo (`type`, `press`, `scroll`, `cdp`)
  devuelven un error explícito en vez de fallar en silencio.
- **Workspace completo de Wisp, SSRF y agentes unificados**: añadidos los crates `wisp-dom` (`Page -> Navigable -> Document` y orígenes), `wisp-css` (`default.css` UA + parser CSS + `var()`/`calc()`/`min()`/`max()`/`clamp()`), `wisp-style` (`StyleComputer` con buckets), `wisp-layout` (BFC/IFC/FFC/TFC + `LayoutState`), `wisp-net` (bloqueo SSRF en URL principal, redirecciones, subrecursos y `fetch()` JS + presupuestos duros) y `wisp-benchmark` (6 tracks + `scripts/run-all.sh`, `WISP_RESULTS.md` y `deny.toml`).
- **MCP unificado, `@ref` snapshots y paridad Ghost↔`wisp-agent`**: `wisp-mcp` añade las 14 herramientas `browser_*`, `wisp-engine` genera snapshots `@e1`, `@e2`… con centralidad, `wisp-agent` incorpora dispatcher CDP headless (incl. `LP.getMarkdown`), exportación PDF 1.4 y flags `--workers`/`--allow-private-network`, y `src/browser.rs` habilita `eval`, `snapshot`, `click` por `@ref`, `move` y `fill` también en pestañas Wisp.
- **VM JS propia y bindings WebIDL**: `wisp-js::vm` añade pipeline `lexer -> parser -> AST -> bytecode -> VM` con presupuesto de pasos, y `wisp-idl/build.rs` genera `JS_IDL_BINDINGS`.
- **Captura en `wisp-agent`**: el motor rasteriza su lista de pintura a un PNG
  (fondos, bordes, degradados, texto y recorte) sin añadir un codificador al
  binario: el PNG se escribe con deflate «stored» y CRC/Adler-32 propios. Se
  expone como `POST /tabs/{id}/screenshot` (con `path` opcional y guardado por
  defecto en `%TEMP%/ghost_shots/`) y como `wisp-agent screenshot <url>` por
  línea de comandos. Es lo que deja a un agente **ver** la página y elegir las
  coordenadas que luego usa `click`/`move`.

### Corregido
- Los botones de borrado de historial por período (24 h / 7 días) y de
  caché/cookies ya no son marcadores de posición: hacen lo que prometen.
- La página de inicio `ghost://newtab` envía la consulta cruda a Ghost en vez
  de construir una URL fija de DuckDuckGo, así respeta el motor elegido.

## [0.1.2] - 2026-11-17

### Añadido
- **API de control remoto** en `127.0.0.1:47820` (`GHOST_REMOTE_PORT`):
  listar pestañas, abrir, navegar, atrás/adelante/recargar/cerrar/activar,
  evaluar JavaScript en WebView2 y leer el texto de cualquier pestaña.
- **`ghost-mcp`**: puente MCP (Model Context Protocol) por stdio para que
  agentes de IA controlen Ghost como herramienta nativa
  (`cargo build --release --bin ghost-mcp`).

### Corregido
- **Congelación de SPAs** (YouTube, Gmail): los eventos de historial
  (`pushState`/`replaceState`) ya no escriben la sesión completa a disco
  por cada evento; saturaban el hilo de UI y congelaban el lazy-load.
- La pestaña activa nunca baja a `MemoryUsageLevel::Low`: el nivel bajo
  estrangulaba timers/rAF y rompía el contenido perezoso del WebView2.

## [0.1.1] - 2026-11-17

### Corregido
- **Maximizar ya no se comporta como F11**: `WM_GETMINMAXINFO` aplica el
  área de trabajo del monitor antes de maximizar; la barra de tareas queda
  accesible en todos los monitores.
- Esquinas de ventana cuadradas al maximizar (evita artefactos visuales
  en los bordes de pantalla).
- Estilo de marco DWP reaplicado al redimensionar.

## [0.1.0] - 2026-11-17

### Añadido
- Primera release pública: `GhostSetup.exe` (Inno Setup, ~10.6 MB,
  instalación sin admin, acceso directo y desinstalador limpio).
- Wisp: motor web experimental en Rust con tipografía real (ab_glyph),
  layout Taffy (flexbox/grid), CSS (custom properties, calc, media queries,
  box sizing, bordes, radios, imágenes, background-image/gradients),
  JavaScript con Boa bajo límites estrictos, módulos ES, caché de
  documentos preparados y modo lectura.
- Ghost: pestañas con restauración bajo demanda, suspensiones LRU,
  conmutación automática Wisp/WebView2 con fallback silencioso, página de
  nueva pestaña nativa, tema claro/oscuro, marcadores, historial,
  descargas, zoom y modo lectura.
- Benchmark de RAM: 229 MB en 1 proceso con 20 pestañas Wisp
  (frente a ~2.4 GB en Edge), medido con `ram_bench.ps1`.
- 139 tests automatizados (wisp + ghost).

[0.1.2]: https://github.com/sarch-codelab/ghost-browser/releases/tag/v0.1.2
[0.1.1]: https://github.com/sarch-codelab/ghost-browser/releases/tag/v0.1.1
[0.1.0]: https://github.com/sarch-codelab/ghost-browser/releases/tag/v0.1.0
