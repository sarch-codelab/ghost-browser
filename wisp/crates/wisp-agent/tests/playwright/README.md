# Prueba de conexión Playwright → `wisp-agent serve`

`chromium.connectOverCDP` de Playwright contra el endpoint CDP del motor
headless, sin Chromium instalado.

```powershell
cargo build -p wisp-agent
cd crates/wisp-agent/tests/playwright
npm install
node cdp-smoke.mjs
```

El fixture vive en `127.0.0.1`, así que el test arranca el servidor con
`--allow-private-network` (la protección SSRF del motor sigue activa para
cualquier otro caso).

`WISP_CDP_TRACE=1` imprime el tráfico CDP del servidor (peticiones, respuestas
y notificaciones) para depurar clientes reales.

## Estado actual

Comprobaciones requeridas (todas en verde): `/health`, apertura de pestaña por
la API HTTP, `connectOverCDP`, `browser.contexts()`, `context.pages()` y
**`page.goto()`**.

El ciclo de vida de frames imita a Chrome:

- el frame principal usa el mismo identificador que el target de la página
  (`tab-N`), que es como Playwright indexa sus sesiones por frame;
- `Page.enable` publica `Runtime.executionContextCreated` con
  `auxData.frameId`/`isDefault`, necesario para evaluar en el documento;
- cada navegación emite `Page.frameNavigated` (con `loaderId` y
  `securityOrigin`), el contexto nuevo, `Page.lifecycleEvent` para
  `DOMContentLoaded` y `load`, y `Page.loadEventFired`;
- un fallo de carga viaja como `errorText` dentro de una respuesta válida, no
  como error de protocolo;
- `Page.createIsolatedWorld` devuelve un `executionContextId`.

Pendiente: `page.title()`, `locator()` y `page.screenshot()`. Playwright
instala un script de utilidades en el contexto de la página y el motor todavía
no ejecuta todos los constructos que usa (ya soporta cierres con captura,
clases como expresión, `instanceof`, `new (<expr>)(...)` y var multi-declarador).
Esas comprobaciones están marcadas como diagnóstico: se informan, no hacen
fallar el test.
