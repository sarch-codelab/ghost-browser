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

## Huella de disco y `LNK1180` en Windows

La raíz y `wisp/` mantienen manifiestos separados; sin configuración adicional,
`target/` (~10,9 GB) y `wisp/target/` (~14,4 GB) pueden superar los 25 GB en
Windows por los PDBs de `debug = 2` y causar `fatal error LNK1180`.

- Ambos `Cargo.toml` fijan `[profile.dev.package."*"] debug = 0` y
  `debug = "line-tables-only"` en el código propio para reducir la huella en
  disco por debajo de ~6 GB.
- Para evitar compilar `wisp-engine` y sus dependencias dos veces al alternar
  entre raíz y `wisp/`, exporta un directorio compartido:

  ```powershell
  $env:CARGO_TARGET_DIR = "$PWD\.cargo-target"
  ```

- Si el disco queda justo, limpia artefactos previos con `cargo clean` y
  `cargo clean --manifest-path wisp/Cargo.toml`.

## Arquitectura (mapa rápido)

- `src/main.rs` — bucle de eventos winit, ventana borderless Win32, egui+wry.
- `src/browser.rs` — pestañas, motores, historial, suspensiones, API remota.
- `src/webview.rs` — WebView2: init script, privacidad, bounds, memoria.
- `src/ui.rs` — chrome de la interfaz egui y pintado de Wisp.
- `src/remote.rs` — API HTTP localhost para control programático.
- `wisp/crates/wisp-mcp/` — puente MCP (stdio JSON-RPC) para agentes de IA; sirve tanto a Ghost como a `wisp-agent`.
- `wisp/` — motor experimental: parser HTML, CSS, Taffy, Boa, pintado.
- `installer/ghost.iss` — Inno Setup; `ram_bench.ps1` — benchmark de RAM.

## Áreas donde más ayuda hace falta

- Cobertura WPT y test262 en Wisp (sustituir `taffy`, `scraper`/`html5ever` y `boa_engine` por los crates propios `wisp-layout`, parser HTML propio y `wisp-js::vm`).
- Transporte WebSocket CDP completo para conectar `puppeteer-core` / `playwright-core` contra `wisp-agent`.
- Benchmarks reproducibles con `scripts/run-all.sh` y actualización de `WISP_RESULTS.md`.
- Firma de código para el instalador (Azure Trusted Signing).
