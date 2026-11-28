# WISP_RESULTS.md — Resultados medidos de Wisp

Mediciones de `wisp-benchmark --track all` en linux x86_64. El motor es propio (`wisp-js::vm`
en Rust puro, sin V8 ni Boa).

| Métrica | Wisp (medido) |
|---|---|
| WPT Core — muestra embebida propia (301 casos escritos a mano) | 300/301 (99.7 %) |
| WPT Full — muestra embebida propia (1.001 casos escritos a mano) | 1000/1001 (99.9 %) |
| React — script propio, en frío | 0.46 ms / 7.1 MB |
| Preact — script propio | 0.37 ms / 7.1 MB |
| Vue — script propio | 0.44 ms / 7.4 MB |
| 33 fixtures modern-web | 33/33, mediana 0.05 ms |
| 4 workers (layout) | 80171.2 pg/s, 8.5 MB |
| Corpus sintético (98 páginas embebidas) | 98/98, 0.00 s |
| 1.500 URLs (corpus repetido) | 1500/1.500: 0 crashes, 0 panics |

> Nota metodológica: estos tracks son suites y fixtures sintéticos embebidos en
> `wisp-benchmark`; no ejecutan el repositorio completo de WPT ni el corpus vivo de
> ningún otro motor. Las filas «WPT» cuentan casos escritos a mano, no subtests de
> WPT, y las de React/Preact/Vue miden un script propio que crea unos pocos nodos,
> no los frameworks. Estas cifras sirven para seguir la evolución de Wisp. RSS del proceso: 8.5 MB.
