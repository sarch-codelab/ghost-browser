# Wisp Engine

Wisp is an experimental Rust web engine workspace (`wisp-engine` + modular crates).
It powers both Ghost's optional in-process engine and the standalone headless
`wisp-agent` binary (zero UI/WebView dependencies).

## Workspace structure

- `wisp-dom`: `Page -> Navigable -> Document` model, origins (`Opaque` / `Http`), history, iframes, and `document.write()`.
- `wisp-css`: User-Agent stylesheet (`default.css`), custom CSS parser (`CssSheet`, `@media`, `@supports`), `var()`, `calc()`, `min()`, `max()`, `clamp()`, and specificity.
- `wisp-style`: `StyleComputer` with right-most selector buckets (`#id`, `.class`, `tag`, `*`) and `UA -> Author -> Inline` cascade.
- `wisp-layout`: own layout IR and formatting contexts (`Block`, `Inline`, `Flex`, `Table`), `LayoutState`, CSS 2.1 margin collapse, and anonymous block wrapping (running alongside `taffy` during migration).
- `wisp-paint`: immutable `PaintCommand` display list (`FillRect`, `StrokeRect`, `Text`, `Image`, `BackgroundImage`, `Clip`/`ClipEnd`).
- `wisp-js`: bounded Boa runtime + `dom.js` + native bytecode VM (`wisp_js::vm`, lexer -> parser -> AST -> bytecode -> stack VM).
- `wisp-idl`: build-time WebIDL parser generating Rust descriptors (`INTERFACES`) and JS prototype/constant bindings (`JS_IDL_BINDINGS`).
- `wisp-net`: SSRF guardrails (private/loopback/link-local/metadata IP & DNS-rebinding checks, `--allow-private-network`) and hard deadline budgets (`WISP_SCRIPT_DEADLINE_MS`, `WISP_MODULE_BUDGET_MS`, `WISP_FETCH_DEADLINE_MS`).
- `wisp-cdp`: CDP helper for WebView2 hosts.
- `wisp-mcp`: shared HTTP control contract (`control`) and stdio JSON-RPC 2.0 MCP server (`browser_*` and `ghost_*` tools).
- `wisp-agent`: headless CLI (`fetch`, `screenshot`, `serve`) with PNG rasterizer, PDF 1.4 export, `@ref` snapshots, headless CDP dispatcher, SSRF protection, and `--workers` pool.
- `wisp-benchmark`: 6-track benchmark harness (`wpt`, `obstacle`, `compare`, `realworld`, `reliability`, `perf`; invoked via `scripts/run-all.sh`).

## Run the prototype & headless agent

From the repository root:

```powershell
cargo run --manifest-path wisp/Cargo.toml -- wisp/examples/hello.html 800
cargo run --manifest-path wisp/Cargo.toml -- wisp/examples/dom.html 800
cargo run --manifest-path wisp/Cargo.toml -p wisp-agent -- fetch https://example.com --dump markdown
cargo run --manifest-path wisp/Cargo.toml -p wisp-agent -- screenshot https://example.com
cargo run --manifest-path wisp/Cargo.toml -p wisp-agent -- serve --port 47820 --workers 4
```

`hello.html` covers inline text updates; `dom.html` creates nodes, attaches a
click listener, and runs a timer. The CLI output contains CSS-pixel rectangles
and text commands. It is an inspectable rendering plan, not a browser window or
a production renderer.

## Current scope

- HTML5 error recovery and a limited DOM-to-layout conversion.
- Block flow, approximate text wrapping, inline styles, and a cascade with
  real CSS specificity (IDs, classes/attributes/pseudo-classes and element
  counts) resolved in source order. Supported declarations include `display`,
  `color`, `background`, `font-size`, `font-weight`, `line-height`,
  `text-align`, `text-transform`, `text-decoration` (underline), `white-space`,
  `width`, `height`, `margin`, `padding`, `border`, `border-radius`,
  `box-sizing`, and the flex/grid properties listed below.
- Responsive CSS: `@media` queries are evaluated against the tab width
  (`min-width`, `max-width`, `width`, media types, `orientation` and the
  `prefers-*` preferences), and CSS custom properties
  (`var(--token, fallback)`) inherit through the DOM like in a browser.
- CSS units: `px`, `em`, `rem`, `pt`, `pc`, `%` and `calc()` expressions that
  mix percentages with absolute units, plus `%`/`em` font sizes, so typography
  can scale from `:root`.
- CSS math functions: `min()`, `max()` and `clamp()` over longitudes,
  percentages and nested functions, resolved against the container at layout
  time for `width`, `height`, `min`/`max-*`, `margin`, `padding`, `gap`,
  `border-radius` and `flex-basis`.
- Box constraints: `min-width`/`max-width`/`min-height`/`max-height` and
  horizontal `margin: auto` (the classic centered column) take part in layout,
  including flex and grid items.
- `aspect-ratio` (`16 / 9`, `1.5`) sizes the missing axis from the defined
  one, honouring `box-sizing`, and works for block, flex and grid items.
- `list-style`/`list-style-type` (`disc`, `circle`, `square`, `none`,
  `decimal`, `lower`/`upper-alpha`, `lower`/`upper-roman`),
  `visibility: hidden` (keeps its space without painting) and
  `overflow-wrap: break-word` for long words.
- Per-side borders (`border-top`/`border-left`…, `border-*-width/style/color`
  and 1–4 value shorthands) painted as `solid`, `dashed` or `dotted`;
  `<hr>` renders a rule and inline `<a>` links are underlined.
- Inline text honours `line-height` (text is centred inside its line box),
  `white-space: pre | pre-wrap | nowrap` (including `<pre>` blocks) and
  hard `<br>` breaks.
- Ghost fetches HTTP/HTTPS HTML, linked CSS, `<img>` images (including base64
  `data:` URIs) and scripts/modules with per-request, count, and
  aggregate-size limits; the CLI accepts local HTML files only. The whole page
  shares an 8 s network budget: once it is spent, remaining resources are
  skipped and Ghost reports how many were not downloaded instead of chaining
  per-request timeouts.
- `<img>` elements participate in inline layout like replaced elements:
  CSS `width`/`height` win over the `width`/`height` attributes, which win
  over the intrinsic size read from the image header. Images inside links
  stay clickable, and missing images fall back to their `alt` text.
- Boa executes bounded inline classic scripts against a mutable DOM built from
  the parsed document. It supports node creation, insertion, removal and
  cloning; attributes, `classList`, `dataset`, `style` and `innerHTML`;
  `querySelector`/`querySelectorAll`; bubbling events with `addEventListener`;
  timers, microtasks and animation frames; `location`, `history`,
  `localStorage`/`sessionStorage`, `matchMedia`, and small `navigator`/
  `performance`/`console` surfaces.
- Basic Web Components: `customElements.define()`/`get()`/`whenDefined()`/
  `upgrade()` and the `connectedCallback`, `disconnectedCallback` and
  `attributeChangedCallback` (with `observedAttributes`) lifecycle hooks, both
  for elements already in the document and for ones created later. `attachShadow`
  falls back to the element's own children so the component still lays out.
- The script stage runs once per page while CSS collection, media-query
  selection and DOM conversion are cached per viewport width, so resizing a
  tab re-evaluates responsive rules without re-running JavaScript.
- `fetch()` and `XMLHttpRequest` are available to page scripts: the request is
  performed by Rust (Boa has no I/O) with the engine's usual limits — HTTP(S)
  only, no embedded credentials, GET/HEAD, 512 KiB per response, 24 requests
  per page, 6 s of total network time and 5 s per request — and cross-origin
  reads require `Access-Control-Allow-Origin`, like a browser. When CORS or a
  limit blocks a request, `fetch()` rejects with the reason (not a silent
  failure) and Ghost lists it with the page warnings.
  Responses expose `text()`, `json()`, `status`, `ok`, `url` and
  `headers.get()`.
- JavaScript limits: 64 KiB per inline script, 256 KiB of inline script source
  per page, 32 scripts, 100,000 loop iterations, recursion depth 128, 20,000 DOM
  nodes, 100 timer callbacks, 9 interval repeats, 10 animation frames, 8 runtime
  warnings, and an 8 MiB snapshot limit. Wisp does not define WebSocket or
  unrestricted network/system APIs.
- External classic scripts and ES modules (including relative imports from a
  pre-fetched module graph) run inside Boa's bounded runtime.
- Flexbox and CSS Grid run through Taffy: `display: flex|grid`,
  `flex-direction`, `flex-wrap`, `justify-content`, `align-items`, `gap`,
  `flex-grow`/`flex-shrink`/`flex-basis` on items, and
  `grid-template-columns`/`grid-template-rows` with `px`, `%`, `fr`, `auto`,
  `minmax()`, `repeat(N, ...)`, `repeat(auto-fill, ...)` and
  `repeat(auto-fit, ...)`. Every child of a grid container is a grid item
  (inline children are blockified) and `grid-column`/`grid-row` accept lines
  and spans (`2`, `1 / 3`, `span 2`, `2 / span 3`); `grid-area` is not
  supported.
- Visual CSS: `font-weight` (bold via a registered bold font in Ghost),
  `text-align` (left/center/right per line), `box-sizing`
  (content-box/border-box), and percentage lengths for `width`, `height`,
  `margin`, `padding`, `gap`, and `flex-basis` (percent `height` resolves as
  auto, matching browsers when the parent height is indefinite).
- Box model complete: `border`/`border-width`/`border-color`/`border-style`/
  `border-radius` (px and %, clamped), `box-sizing`, and `text-align`.
  `ul`/`ol`/`li` render with indentation, bullets and ordered numbers.
- `overflow: hidden|clip|scroll|auto|overlay` clips the content to the padding
  box. The paint list carries nested `Clip`/`ClipEnd` commands, so Ghost crops
  cards, avatars and scroll panels exactly like a browser.
- `<img srcset>` is honoured when there is no `src`: Wisp fetches and paints the
  candidate with the largest descriptor instead of dropping the image.
- Conditional CSS: `@media` and `@supports` blocks, including `not`, `and`,
  `or`, `(propiedad: valor)` and `selector(...)`. `@supports` is evaluated
  against what the engine actually implements, so fallbacks keep working.
- Still missing: forms, accessibility tree, `<picture>`/`sizes` selection,
  `@container`/`@import` inside `@supports`, `grid-area` and named grid lines,
  `display: contents`, and full CSS cascade (`display` values beyond
  block/inline/flex/grid/none, most pseudo-classes). Many modern sites remain
  incomplete or unusable.
- Input size and nesting limits for this experimental implementation.


Ghost keeps WebView2 as its default engine; Wisp is an experimental engine
selectable per tab from the browser UI.

Wisp's long-term goal is to prioritize stability and efficiency in the features
it supports. Any performance or stability advantage over WebView2 remains an
unverified goal and needs reproducible benchmarks.
