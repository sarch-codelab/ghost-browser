<div align="center">
  <img src="ghost-mark.png" alt="Logotipo de Ghost" width="128">
  <h1>Ghost Browser</h1>
  <p>Un navegador de escritorio ligero, hecho con Rust y WebView2.</p>
  <p>
    <a href="https://github.com/sarch-codelab/ghost-browser/releases/latest"><img src="https://img.shields.io/github/v/release/sarch-codelab/ghost-browser" alt="Última release"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/licencia-MIT-blue" alt="Licencia MIT"></a>
    <a href="https://github.com/sarch-codelab/ghost-browser/actions/workflows/release.yml"><img src="https://github.com/sarch-codelab/ghost-browser/actions/workflows/release.yml/badge.svg" alt="Build"></a>
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
  por URL.
- **Nueva pestaña nativa:** la página de inicio se pinta con la propia
  interfaz sin arrancar Chromium.
- **Privacidad:** filtrado opcional de dominios conocidos en navegación y
  solicitudes `fetch`/XHR, pausa de medios en segundo plano y opción para borrar
  datos de navegación. No es un bloqueador exhaustivo.
- **Marcadores e historial:** consultar y administrar ambos desde la interfaz.
- **Descargas:** elegir la carpeta de destino; Ghost añade sufijos a nombres
  repetidos cuando hay un nombre disponible.
- **Apariencia:** temas claro y oscuro, además de una página integrada para
  nuevas pestañas.
- **Herramientas:** página de inicio configurable, búsqueda con DuckDuckGo,
  zoom y herramientas de desarrollo.

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

Si no se encuentra `rc.exe`, instala el Windows SDK o define la variable de
entorno `RC` con la ruta completa al compilador de recursos.

## Atajos de teclado

| Atajo | Acción |
| --- | --- |
| `Ctrl+T` | Abrir una pestaña |
| `Ctrl+W` | Cerrar la pestaña actual |
| `Ctrl+Shift+T` | Reabrir la última pestaña cerrada |
| `Ctrl+L` | Enfocar la barra de direcciones |
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

## Control programático y agentes de IA

Ghost expone una **API HTTP local** para que otras herramientas y agentes de
IA conduzcan el navegador. Escucha solo en `127.0.0.1` (ningún equipo externo
puede conectarse) en el puerto `47820`, configurable con la variable de
entorno `GHOST_REMOTE_PORT`.

| Método y ruta | Acción |
| --- | --- |
| `GET /health` | Comprueba que la API responde |
| `GET /tabs` | Lista pestañas: id, título, url, motor, estado |
| `POST /tabs` | `{"url": "..."}` abre una pestaña nueva |
| `POST /tabs/{id}/navigate` | `{"url": "..."}` navega la pestaña |
| `POST /tabs/{id}/back` `/forward` `/reload` `/close` `/activate` | Acciones de historial y gestión |
| `POST /tabs/{id}/eval` | `{"js": "..."}` ejecuta JS en WebView2 y devuelve el resultado |
| `POST /tabs/{id}/text` | Texto legible de la página (reader en Wisp, `innerText` en WebView2) |

Ejemplo:

```powershell
curl http://127.0.0.1:47820/tabs
curl -X POST http://127.0.0.1:47820/tabs/3/eval -d '{\"js\":\"document.title\"}'
```

### `ghost-mcp`: Ghost como herramienta MCP

El paquete incluye un segundo binario, `ghost-mcp`, que habla
[Model Context Protocol](https://modelcontextprotocol.io) por stdio
(JSON-RPC 2.0). Cualquier cliente MCP — Claude Desktop, Cursor, Devin,
scripts propios — puede usar Ghost como herramienta nativa sin tocar la API
HTTP directamente:

```powershell
cargo build --release --bin ghost-mcp
```

Configuración MCP de ejemplo:

```json
{
  "mcpServers": {
    "ghost": {
      "command": "C:/ruta/a/ghost-mcp.exe"
    }
  }
}
```

Herramientas expuestas: `ghost_health`, `ghost_tabs`, `ghost_open_tab`,
`ghost_navigate`, `ghost_tab_action` (back/forward/reload/close/activate/
text) y `ghost_eval`.

> **Aviso de seguridad**: la API no tiene autenticación porque solo escucha
> en loopback, pero cualquier proceso local de tu equipo puede controlar el
> navegador mientras Ghost esté abierto. No la expongas a la red.

## Privacidad y datos

El bloqueador integrado utiliza una lista limitada de patrones de dominios
conocidos; no es un bloqueador de contenido exhaustivo ni hace anónima la
navegación. Los sitios que visitas siguen recibiendo solicitudes de tu
dispositivo.

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
cargo test --bin ghost
cargo test -p wisp-engine
```

El código principal está en `src/` (`browser.rs`, `webview.rs`, `ui.rs`,
`remote.rs`) y el motor experimental en `wisp/`. `build.rs`, `ghost.rc` y
`ghost.ico` configuran el recurso de Windows.

Para contribuir, lee [CONTRIBUTING.md](CONTRIBUTING.md). Para reportar una
vulnerabilidad de seguridad, sigue [SECURITY.md](SECURITY.md) — no abras un
issue público. El historial de versiones está en [CHANGELOG.md](CHANGELOG.md).

## Rendimiento y memoria

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

## Wisp experimental

Wisp está integrado como motor **Beta** seleccionable por pestaña; WebView2
sigue siendo el predeterminado para webapps. Wisp solicita páginas HTTP/HTTPS, hojas CSS,
imágenes y scripts dentro de límites, y aplica una cascada CSS con
especificidad real que incluye consultas `@media` según el ancho de la pestaña,
variables `var(--token)`, unidades `em`/`rem`/`%`, `calc()`, `min`/`max-width`,
márgenes `auto`, `line-height`, `white-space`, listas, bordes por lado y
flex/grid. Los scripts inline, externos y los
módulos ES se ejecutan sobre un DOM mutable con eventos, temporizadores,
`localStorage`, `location` y `history` básicos; los fallos de JavaScript se
muestran como advertencias sin impedir el renderizado estático. No implementa
todas las APIs de JavaScript o CSS (formularios, `fetch`, XHR, `overflow`),
así que las webs modernas e interactivas pueden verse incompletas:
cambia esa pestaña a WebView2 para usarlas.

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
