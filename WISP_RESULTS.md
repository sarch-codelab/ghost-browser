# WISP_RESULTS.md — Tabla de victoria diaria

Comparativa continua de Wisp frente a Ladybird y Obscura.
Generada/actualizada por `wisp-benchmark` en CI (`--track all --victory-table WISP_RESULTS.md`).

| Métrica | Ladybird | Obscura | Objetivo Wisp | Wisp actual |
|---|---|---|---|---|
| WPT Core | wpt.fyi | **83,3 %** | **> 83,3 %** | generado en CI (`wisp-benchmark --track wpt`) |
| WPT Full | motor completo | 67,6 % | **> 67,6 %** | generado en CI (`wisp-benchmark --track wpt`) |
| React, frío | — | 81 ms / 30 MB | **< 81 ms / < 30 MB** | generado en CI (`wisp-benchmark --track compare`) |
| Preact | — | 45 ms / 30 MB | **< 45 ms / < 30 MB** | generado en CI (`wisp-benchmark --track compare`) |
| Vue | — | 86 ms / 31 MB | **< 86 ms / < 31 MB** | generado en CI (`wisp-benchmark --track compare`) |
| 33 fixtures modern-web | — | 33/33, mediana 44 ms | **33/33, mediana < 44 ms** | generado en CI (`wisp-benchmark --track obstacle`) |
| 4 workers | — | 40 pg/s, 112 MB | **> 40 pg/s, < 112 MB** | generado en CI (`wisp-benchmark --track perf`) |
| Corpus real | — | 94/98, 5,2 s | **> 94/98, < 5,2 s** | generado en CI (`wisp-benchmark --track realworld`) |
| 1.500 URLs | — | 0 crashes, 0 panics | **0 crashes, 0 panics** | generado en CI (`wisp-benchmark --track reliability`) |
| Motor propio | Sí | No (usa V8) | **Sí (sin V8 ni Boa)** | Sí (`wisp-js::vm` nativo en Rust puro; sin V8 ni Boa) |

> Nota metodológica: los tracks de `wisp-benchmark` miden suites y fixtures sintéticos/embebidos propios de Wisp (no ejecutan el repositorio externo completo de WPT ni el corpus vivo de red de Obscura en CI offline).
