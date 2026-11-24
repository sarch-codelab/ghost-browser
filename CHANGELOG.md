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
