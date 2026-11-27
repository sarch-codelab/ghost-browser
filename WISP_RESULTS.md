# WISP_RESULTS.md — Tabla de victoria diaria

Comparativa continua de Wisp frente a Ladybird y Obscura.
Generada por `wisp-benchmark --track all --victory-table WISP_RESULTS.md`.
Las cifras de la columna «Wisp (medido)» son de esta máquina; las de Ladybird y
Obscura provienen de sus informes públicos y miden otras suites (ver nota).

| Métrica | Ladybird | Obscura | Objetivo Wisp | Wisp (medido) |
|---|---|---|---|---|
| WPT Core (muestra embebida propia) | wpt.fyi | **83,3 %** | **> 83,3 %** | 300/301 (99.7 %) |
| WPT Full (muestra embebida propia) | motor completo | 67,6 % | **> 67,6 %** | 1000/1001 (99.9 %) |
| React — script propio, frío | — | 81 ms / 30 MB | **< 81 ms / < 30 MB** | 0.77 ms / 8.7 MB |
| Preact — script propio | — | 45 ms / 30 MB | **< 45 ms / < 30 MB** | 0.58 ms / 8.7 MB |
| Vue — script propio | — | 86 ms / 31 MB | **< 86 ms / < 31 MB** | 0.64 ms / 8.9 MB |
| 33 fixtures modern-web | — | 33/33, mediana 44 ms | **33/33, mediana < 44 ms** | 33/33, mediana 0.16 ms |
| 4 workers (layout) | — | 40 pg/s, 112 MB | **> 40 pg/s, < 112 MB** | 42719.5 pg/s, 10.2 MB |
| Corpus real (98 páginas embebidas) | — | 94/98, 5,2 s | **> 94/98, < 5,2 s** | 98/98, 0.01 s |
| 1.500 URLs (corpus repetido) | — | 0 crashes, 0 panics | **0 crashes, 0 panics** | 1500/1.500: 0 crashes, 0 panics |
| Motor propio | Sí | No (usa V8) | **Sí (sin V8 ni Boa)** | Sí (`wisp-js::vm` nativo en Rust puro) |

> Nota metodológica: estos tracks son suites y fixtures sintéticos embebidos en
> `wisp-benchmark`; no ejecutan el repositorio completo de WPT ni el corpus vivo de
> red de Obscura. Las filas «WPT» cuentan casos escritos a mano (301 y 1001), no
> subtests de WPT, y las de React/Preact/Vue miden un script propio que crea unos
> pocos nodos, no los frameworks. Por eso las cifras de Wisp y las de los rivales
> no son la misma medición: sirven para seguir la evolución de Wisp, no para
> declarar victoria. RSS del proceso: 10.2 MB.
