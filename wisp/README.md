# Wisp Engine

Wisp is an experimental Rust rendering-engine project for Ghost. It fetches
HTTP/HTTPS HTML documents when used by Ghost, parses them with an HTML5 parser,
computes a small block-flow layout, and emits paint commands for Ghost's UI.

## Run the prototype

From the repository root:

```powershell
cargo run --manifest-path wisp/Cargo.toml -- wisp/examples/hello.html 800
cargo run --manifest-path wisp/Cargo.toml -- wisp/examples/dom.html 800
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
- Box constraints: `min-width`/`max-width`/`min-height`/`max-height` and
  horizontal `margin: auto` (the classic centered column) take part in layout,
  including flex and grid items.
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
  aggregate-size limits; the CLI accepts local HTML files only.
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
- After scripts run, Wisp serializes the mutated document and re-parses it so
  the CSS cascade applies to elements created by JavaScript. Documents that
  scripts did not change keep the original parse.
- The script stage runs once per page while CSS collection, media-query
  selection and DOM conversion are cached per viewport width, so resizing a
  tab re-evaluates responsive rules without re-running JavaScript.
- JavaScript limits: 64 KiB per inline script, 256 KiB of inline script source
  per page, 32 scripts, 100,000 loop iterations, recursion depth 128, 20,000 DOM
  nodes, 100 timer callbacks, 9 interval repeats, 10 animation frames, 8 runtime
  warnings, and an 8 MiB snapshot limit. Wisp does not define `fetch`, XHR,
  WebSocket, or unrestricted network/system APIs.
- External classic scripts and ES modules (including relative imports from a
  pre-fetched module graph) run inside Boa's bounded runtime.
- Flexbox and CSS Grid run through Taffy: `display: flex|grid`,
  `flex-direction`, `flex-wrap`, `justify-content`, `align-items`, `gap`,
  `flex-grow`/`flex-shrink`/`flex-basis` on items, and basic
  `grid-template-columns`/`grid-template-rows` (`px`, `%`, `fr`, `auto`,
  `repeat(N, ...)`). Grid item placement is automatic only; percentages and
  `minmax()`/`auto-fill` are not supported yet.
- Visual CSS: `font-weight` (bold via a registered bold font in Ghost),
  `text-align` (left/center/right per line), `box-sizing`
  (content-box/border-box), and percentage lengths for `width`, `height`,
  `margin`, `padding`, `gap`, and `flex-basis` (percent `height` resolves as
  auto, matching browsers when the parent height is indefinite).
- Box model complete: `border`/`border-width`/`border-color`/`border-style`/
  `border-radius` (px and %, clamped), `box-sizing`, and `text-align`.
  `ul`/`ol`/`li` render with indentation, bullets and ordered numbers.
- Still missing: forms, accessibility tree, `srcset`/`<picture>` selection,
  `@supports`/`@container`/`@import`, `min()`/`max()`/`clamp()`,
  `overflow` clipping, `minmax()`/`auto-fill` grid tracks, explicit grid
  placement, and full CSS cascade (`display` values beyond
  block/inline/flex/grid/none, most pseudo-classes). Many modern sites remain
  incomplete or unusable.
- Input size and nesting limits for this experimental implementation.


Ghost keeps WebView2 as its default engine; Wisp is an experimental engine
selectable per tab from the browser UI.

Wisp's long-term goal is to prioritize stability and efficiency in the features
it supports. Any performance or stability advantage over WebView2 remains an
unverified goal and needs reproducible benchmarks.
