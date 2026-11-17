# Contribuir a Ghost Browser

¡Gracias por tu interés en Ghost! Este documento explica cómo proponer
cambios, reportar problemas y mantener la calidad del proyecto.

## Código de conducta

Sé respetuoso, constructivo y enfocado en el problema técnico. El acoso, el
spam y los ataques personales no se toleran. Si ves un comportamiento que lo
viola, abre un issue o contacta al mantenedor.

## Reportar un bug

Antes de abrir un issue:

1. Busca en los [issues existentes](../../issues) por si ya está reportado.
2. Anota los pasos exactos para reproducirlo.
3. Indica tu versión de Ghost, de Windows y del WebView2 Runtime
   (`edge://settings/help` en Edge muestra la versión).

Incluye en el issue:

- **Qué esperabas** que pasara y **qué pasó** en realidad.
- Pasos para reproducirlo.
- Si es de Wisp: la URL o un fragmento HTML mínimo que lo dispare.
- Si es de la interfaz: captura de pantalla si ayuda.

## Proponer cambios (pull request)

1. Haz un fork del repositorio y crea una rama descriptiva
   (`fix/youtube-lazy-load`, `feat/wisp-grid-gap`...).
2. Sigue las convenciones del código existente: Rust idiomático, comentarios
   en español solo cuando aporten contexto, código compacto.
3. **Toda funcionalidad nueva lleva tests.** El proyecto tiene una suite
   amplia (`cargo test`); manténla verde.
4. Ejecuta antes de enviar:

   ```powershell
   cargo fmt --all
   cargo check --all-targets
   cargo test
   ```

5. Describe en el PR el *por qué* del cambio, no solo el *qué*.

## Arquitectura (mapa rápido)

- `src/main.rs` — bucle de eventos winit, ventana borderless Win32, egui+wry.
- `src/browser.rs` — pestañas, motores, historial, suspensiones, API remota.
- `src/webview.rs` — WebView2: init script, privacidad, bounds, memoria.
- `src/ui.rs` — chrome de la interfaz egui y pintado de Wisp.
- `src/remote.rs` — API HTTP localhost para control programático.
- `src/bin/ghost-mcp.rs` — puente MCP (stdio JSON-RPC) para agentes de IA.
- `wisp/` — motor experimental: parser HTML, CSS, Taffy, Boa, pintado.
- `installer/ghost.iss` — Inno Setup; `ram_bench.ps1` — benchmark de RAM.

## Áreas donde más ayuda hace falta

- Compatibilidad CSS/JS en Wisp (selectores, pseudo-clases, layout).
- Acciones de entrada en la API remota (click, tipeo, scroll).
- Screenshot/captura en la API remota.
- Benchmarks en más escenarios (JS pesado, video, muchas pestañas WebView2).
- Firma de código para el instalador (Azure Trusted Signing).
