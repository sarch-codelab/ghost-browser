# WISP_RESULTS.md — Resultados medidos de Wisp

Mediciones de `wisp-benchmark --track all` en windows x86_64. El motor es propio (`wisp-js::vm`
en Rust puro, sin V8 ni Boa).

| Métrica | Wisp (medido) |
|---|---|
| WPT Core — muestra embebida propia (301 casos escritos a mano) | 300/301 (99.7 %) |
| WPT Full — muestra embebida propia (1.001 casos escritos a mano) | 1000/1001 (99.9 %) |
| React — script propio, en frío | 0.75 ms / 8.7 MB |
| Preact — script propio | 0.57 ms / 8.7 MB |
| Vue — script propio | 0.68 ms / 8.9 MB |
| 33 fixtures modern-web | 33/33, mediana 0.15 ms |
| 4 workers (layout) | 52748.2 pg/s, 10.3 MB |
| Corpus sintético (98 páginas embebidas) | 98/98, 0.01 s |
| 1.500 URLs (corpus repetido) | 1500/1.500: 0 crashes, 0 panics |

> Nota metodológica: estos tracks son suites y fixtures sintéticos embebidos en
> `wisp-benchmark`; no ejecutan el repositorio completo de WPT ni el corpus vivo de
> ningún otro motor. Las filas «WPT» cuentan casos escritos a mano, no subtests de
> WPT, y las de React/Preact/Vue miden un script propio que crea unos pocos nodos,
> no los frameworks. Estas cifras sirven para seguir la evolución de Wisp. RSS del proceso: 10.3 MB.
