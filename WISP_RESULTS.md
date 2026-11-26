# WISP_RESULTS.md — Tabla de victoria diaria

Comparativa continua de Wisp frente a Ladybird y Obscura.
Generada/actualizada por `scripts/run-all.sh` (`wisp-benchmark`).

| Métrica | Ladybird | Obscura | Objetivo Wisp | Wisp actual |
|---|---|---|---|---|
| WPT Core | wpt.fyi | **83,3 %** | **> 83,3 %** | _pendiente de ejecución en CI/cargo_ |
| WPT Full | motor completo | 67,6 % | **> 67,6 %** | _pendiente de ejecución en CI/cargo_ |
| React, frío | — | 81 ms / 30 MB | **< 81 ms / < 30 MB** | _pendiente de ejecución en CI/cargo_ |
| Preact | — | 45 ms / 30 MB | **< 45 ms / < 30 MB** | _pendiente de ejecución en CI/cargo_ |
| Vue | — | 86 ms / 31 MB | **< 86 ms / < 31 MB** | _pendiente de ejecución en CI/cargo_ |
| 33 fixtures modern-web | — | 33/33, mediana 44 ms | **33/33, mediana < 44 ms** | _pendiente de ejecución en CI/cargo_ |
| 4 workers | — | 40 pg/s, 112 MB | **> 40 pg/s, < 112 MB** | _pendiente de ejecución en CI/cargo_ |
| Corpus real | — | 94/98, 5,2 s | **> 94/98, < 5,2 s** | _pendiente de ejecución en CI/cargo_ |
| 1.500 URLs | — | 0 crashes, 0 panics | **0 crashes, 0 panics** | _pendiente de ejecución en CI/cargo_ |
| Motor propio | Sí | No (usa V8) | **Sí (sin V8 ni Boa)** | En migración (`boa-fallback` temporal en `wisp-js`) |
