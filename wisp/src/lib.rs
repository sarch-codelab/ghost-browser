use ab_glyph::{Font as _, ScaleFont as _};
use anyhow::{Result, ensure};
use base64::Engine as _;
use ego_tree::{NodeId, NodeRef};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::redirect::Policy;
use scraper::node::Node;
use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use taffy::prelude::*;

mod script;

pub const MAX_DOCUMENT_BYTES: usize = 10 * 1024 * 1024;
const MAX_STYLESHEET_BYTES: usize = 4 * 1024 * 1024;
const MAX_STYLESHEET_COUNT: usize = 12;
const MAX_IMPORT_COUNT: usize = 12;
const MAX_IMPORT_DEPTH: usize = 3;
const MAX_EXTERNAL_SCRIPT_BYTES: usize = 512 * 1024;
const MAX_EXTERNAL_SCRIPT_COUNT: usize = 24;
const MAX_TOTAL_EXTERNAL_SCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODULE_BYTES: usize = 512 * 1024;
const MAX_MODULE_COUNT: usize = 32;
const MAX_TOTAL_MODULE_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODULE_DEPTH: usize = 8;
const MAX_TREE_DEPTH: usize = 256;
/// Tope de caracteres del texto extraído por [`FetchedPage::reader_text`].
const MAX_READER_CHARS: usize = 200_000;
/// Ancho que observa el JavaScript (`innerWidth`, `matchMedia`) al ejecutarse.
const SCRIPT_VIEWPORT_WIDTH: f32 = 1280.0;
/// Ancho usado por el modo lectura para preparar el documento.
const READER_VIEWPORT_WIDTH: f32 = 1280.0;
const MAX_IMAGE_COUNT: usize = 48;
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 16 * 1024 * 1024;
/// Presupuesto total de red de una página. Wisp descarga documento, CSS,
/// scripts, módulos e imágenes en serie: sin un tope global una página pesada
/// podía encadenar decenas de esperas y tardar minutos. Al agotarse, los
/// recursos restantes se omiten con un aviso explícito.
const MAX_NETWORK_TIME: Duration = Duration::from_secs(8);
/// Tope de cada petición individual mientras quede presupuesto de página.
const MAX_NETWORK_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_COLOR: Color = Color {
    r: 255,
    g: 255,
    b: 255,
    a: 255,
};
/// `font-size` de la raíz para resolver `rem` (el inicial de CSS).
const ROOT_FONT_SIZE: f32 = 16.0;

/// The font Wisp measures text with. It is the same Ubuntu-Light file egui
/// embeds for proportional text, so advances match what Ghost paints.
fn wisp_font() -> &'static ab_glyph::FontArc {
    static FONT: OnceLock<ab_glyph::FontArc> = OnceLock::new();
    FONT.get_or_init(|| {
        ab_glyph::FontArc::try_from_vec(include_bytes!("../assets/fonts/Ubuntu-Light.ttf").to_vec())
            .expect("the embedded Wisp font must be valid")
    })
}

fn wisp_bold_font() -> &'static ab_glyph::FontArc {
    static FONT: OnceLock<ab_glyph::FontArc> = OnceLock::new();
    FONT.get_or_init(|| {
        ab_glyph::FontArc::try_from_vec(include_bytes!("../assets/fonts/Ubuntu-Bold.ttf").to_vec())
            .expect("the embedded Wisp bold font must be valid")
    })
}

fn is_bold_weight(weight: u16) -> bool {
    weight >= 600
}

/// Hasher no criptográfico para las cachés de medidas: las claves son
/// palabras cortas y `SipHash` (el `DefaultHasher` de la std) domina el coste
/// de una búsqueda. Es el mismo esquema FX que usan los compiladores para
/// tablas de símbolos: multiplicación y rotación, sin dependencias nuevas.
#[derive(Default, Clone, Copy)]
struct FastHasher(u64);

impl std::hash::Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = self.0;
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let word = u64::from_le_bytes(chunk.try_into().expect("chunk is 8 bytes"));
            hash = (hash.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut tail = [0u8; 8];
            tail[..rest.len()].copy_from_slice(rest);
            hash = (hash.rotate_left(5) ^ u64::from_le_bytes(tail))
                .wrapping_mul(0x517c_c1b7_2722_0a95);
        }
        self.0 = hash;
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.write(&[value]);
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.write(&value.to_le_bytes());
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.write(&value.to_le_bytes());
    }
}

type FastBuildHasher = std::hash::BuildHasherDefault<FastHasher>;
type FastMap<K, V> = HashMap<K, V, FastBuildHasher>;

/// Horizontal advance of `text` at `font_size` pixels, kerning included.
/// Results are memoized: word widths never depend on layout state, and the
/// same strings are re-measured by every Taffy pass and every resize.
pub fn text_width(text: &str, font_size: f32) -> f32 {
    text_width_weighted(text, font_size, false)
}

/// Igual que [`text_width`] pero eligiendo la familia según `font-weight`:
/// Ubuntu-Bold para pesos >= 600, Ubuntu-Light en caso contrario. Medir y
/// pintar con la misma fuente es lo que mantiene alineados los saltos de
/// línea con lo que Ghost dibuja después.
pub fn text_width_weighted(text: &str, font_size: f32, bold: bool) -> f32 {
    if text.is_empty() || font_size <= 0.0 {
        return 0.0;
    }
    // Caché por hilo: el layout corre en un solo hilo y este es el punto más
    // caliente del motor (cada palabra de cada línea pasa por aquí varias
    // veces). Un `Mutex` global obligaba a un lock por palabra y serializaba
    // cualquier medición concurrente; `RefCell` en thread-local lo elimina.
    thread_local! {
        static CACHE: RefCell<FastMap<(u32, bool), FastMap<Box<str>, f32>>> =
            RefCell::new(FastMap::default());
    }
    let key = (font_size.to_bits(), bold);
    let cached = CACHE.with(|cache| {
        cache
            .borrow()
            .get(&key)
            .and_then(|sizes| sizes.get(text))
            .copied()
    });
    if let Some(width) = cached {
        return width;
    }
    let width = measure_text_width(text, font_size, bold);
    CACHE.with(|cache| {
        let mut guard = cache.borrow_mut();
        let sizes = guard.entry(key).or_default();
        if sizes.len() < 100_000 {
            sizes.insert(text.into(), width);
        }
    });
    width
}

fn measure_text_width(text: &str, font_size: f32, bold: bool) -> f32 {
    let font = if bold { wisp_bold_font() } else { wisp_font() };
    let scaled = font.as_scaled(font_size);
    let mut width = 0.0;
    let mut previous = None;
    for character in text.chars() {
        let glyph = scaled.glyph_id(character);
        if let Some(previous) = previous {
            width += scaled.kern(previous, glyph);
        }
        width += scaled.h_advance(glyph);
        previous = Some(glyph);
    }
    width.max(0.0)
}

/// Métricas verticales de la fuente a `font_size`: `(alto natural de línea,
/// línea base)`. Se piden una vez por palabra al centrar y subrayar texto, y
/// `as_scaled` vuelve a resolver tablas de la fuente en cada llamada, así que
/// se memoizan por tamaño (una página usa un puñado de tamaños distintos).
fn font_metrics(font_size: f32) -> (f32, f32) {
    thread_local! {
        static METRICS: RefCell<FastMap<u32, (f32, f32)>> = RefCell::new(FastMap::default());
    }
    let size = font_size.max(1.0);
    let key = size.to_bits();
    if let Some(metrics) = METRICS.with(|cache| cache.borrow().get(&key).copied()) {
        return metrics;
    }
    let scaled = wisp_font().as_scaled(size);
    let metrics = (
        (scaled.ascent() - scaled.descent() + scaled.line_gap()).max(font_size),
        scaled.ascent(),
    );
    METRICS.with(|cache| {
        let mut guard = cache.borrow_mut();
        if guard.len() < 4096 {
            guard.insert(key, metrics);
        }
    });
    metrics
}

/// Natural line height (`ascent - descent + line gap`) at `font_size` pixels.
pub fn text_line_height(font_size: f32) -> f32 {
    font_metrics(font_size).0
}

/// Distancia desde el borde superior de la línea hasta la línea base, usada
/// para pintar el subrayado de los enlaces.
fn text_ascent(font_size: f32) -> f32 {
    font_metrics(font_size).1
}

/// Aplica `text-transform` a una palabra antes de medirla y pintarla.
/// Aplica `text-transform` sin asignar memoria en el caso habitual
/// (`none`), que es el 99 % de las palabras de una página.
fn transformed_text(word: &str, kind: TextTransformKind) -> Cow<'_, str> {
    match kind {
        TextTransformKind::None => Cow::Borrowed(word),
        TextTransformKind::Uppercase => Cow::Owned(word.to_uppercase()),
        TextTransformKind::Lowercase => Cow::Owned(word.to_lowercase()),
        TextTransformKind::Capitalize => {
            let mut characters = word.chars();
            match characters.next() {
                Some(first) => {
                    Cow::Owned(first.to_uppercase().collect::<String>() + characters.as_str())
                }
                None => Cow::Borrowed(""),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaintCommand {
    FillRect {
        rect: Rect,
        color: Color,
        /// `border-radius` en píxeles (0 = esquinas rectas).
        radius: f32,
    },
    /// Borde de una caja (`border`): trazo uniforme del color dado.
    StrokeRect {
        rect: Rect,
        color: Color,
        width: f32,
        radius: f32,
    },
    Text {
        x: f32,
        y: f32,
        content: String,
        color: Color,
        font_size: f32,
        /// CSS `font-weight` (100–900); 400 is normal, 700+ is bold.
        font_weight: u16,
        href: Option<String>,
    },
    Image {
        rect: Rect,
        /// Resolved URL (or full `data:` URI) used as the key into
        /// [`FetchedPage::images`].
        url: String,
        href: Option<String>,
    },
    /// `background-image` de una caja: imagen remota/`data:` o degradado
    /// lineal. Se pinta detrás del contenido, sobre `background-color`.
    BackgroundImage {
        rect: Rect,
        radius: f32,
        image: BgImage,
    },
    /// Abre una región de recorte (`overflow: hidden|auto|scroll`): todo lo
    /// que se pinte hasta el siguiente [`PaintCommand::ClipEnd`] queda
    /// recortado a `rect`. Los recortes se anidan como una pila.
    Clip { rect: Rect },
    /// Cierra la región abierta por el último [`PaintCommand::Clip`].
    ClipEnd,
}

/// Una capa de `background-image` (solo la primera; `background` admite
/// varias separadas por comas).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BgImage {
    /// Referencia `url(...)` ya resuelta contra la URL de la página: clave
    /// http(s) absoluta o URI `data:` completa, igual que [`PaintCommand::Image`].
    Url(String),
    /// `linear-gradient(...)`: `angle_deg` sigue la convención CSS
    /// (0deg = hacia arriba, 90deg = hacia la derecha). Las paradas usan
    /// fracción 0–1 a lo largo del eje del degradado.
    LinearGradient {
        angle_deg: f32,
        stops: Vec<GradientStop>,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct GradientStop {
    /// Posición a lo largo del degradado, 0.0–1.0.
    pub offset: f32,
    pub color: Color,
}

#[derive(Clone, Debug, Serialize)]
pub struct LayoutDocument {
    pub viewport_width: f32,
    pub content_height: f32,
    pub commands: Vec<PaintCommand>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct FetchedPage {
    pub final_url: String,
    pub title: String,
    pub html: String,
    pub stylesheets: Vec<String>,
    /// External classic scripts keyed by their resolved absolute URL.
    pub external_scripts: HashMap<String, String>,
    /// ES modules keyed by their resolved absolute URL (inline modules use a
    /// `#wisp-inline-module-N` fragment on the page URL).
    pub module_scripts: HashMap<String, String>,
    /// Images fetched for `<img>` elements, keyed by resolved URL or `data:` URI.
    pub images: HashMap<String, ImageResource>,
    pub warnings: Vec<String>,
    /// HTML con los scripts ya ejecutados, una sola vez por página.
    scripted: Arc<OnceLock<Result<ScriptedDocument, String>>>,
    /// DOM preparado por ancho de viewport. Las media queries dependen del
    /// ancho, así que un redimensionado solo reevalúa CSS y conversión, nunca
    /// el JavaScript.
    prepared: Arc<Mutex<HashMap<u32, Arc<PreparedDocument>>>>,
}

#[derive(Clone, Debug)]
struct ScriptedDocument {
    html: String,
    warnings: Vec<String>,
}

/// Raw bytes and intrinsic dimensions of a fetched image.
#[derive(Clone, Debug)]
pub struct ImageResource {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Document state after scripts ran once: the converted DOM roots (parse +
/// CSS cascade + DOM conversion already applied) plus script warnings.
/// Cached per [`FetchedPage`] so resizes only redo block layout.
#[derive(Clone, Debug)]
struct PreparedDocument {
    roots: Vec<DomNode>,
    warnings: Vec<String>,
}

impl FetchedPage {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        final_url: String,
        title: String,
        html: String,
        stylesheets: Vec<String>,
        external_scripts: HashMap<String, String>,
        module_scripts: HashMap<String, String>,
        images: HashMap<String, ImageResource>,
        warnings: Vec<String>,
    ) -> Self {
        Self {
            final_url,
            title,
            html,
            stylesheets,
            external_scripts,
            module_scripts,
            images,
            warnings,
            scripted: Arc::new(OnceLock::new()),
            prepared: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Runs the script stage once per page and re-runs only CSS + DOM
    /// conversion when the viewport width changes (media queries are
    /// width-dependent). Scripts always observe [`SCRIPT_VIEWPORT_WIDTH`], so
    /// `matchMedia`-driven markup stays stable while the window resizes.
    pub fn layout(&self, viewport_width: f32) -> Result<LayoutDocument> {
        let prepared = self
            .prepared_document(viewport_width)
            .map_err(|error| anyhow::anyhow!(error))?;
        let mut document = layout_roots(&prepared.roots, viewport_width, &self.images)?;
        document.warnings = prepared
            .warnings
            .iter()
            .cloned()
            .chain(document.warnings)
            .collect();
        Ok(document)
    }

    fn scripted_document(&self) -> Result<ScriptedDocument, String> {
        self.scripted
            .get_or_init(|| {
                let document = Html::parse_document(&self.html);
                let script_result = script::execute_scripts(
                    &document,
                    &self.external_scripts,
                    &self.module_scripts,
                    Some(&self.final_url),
                    SCRIPT_VIEWPORT_WIDTH,
                )
                .map_err(|error| error.to_string())?;
                Ok(ScriptedDocument {
                    html: script_result.html.unwrap_or_else(|| self.html.clone()),
                    warnings: script_result.warnings,
                })
            })
            .clone()
    }

    fn prepared_document(&self, viewport_width: f32) -> Result<Arc<PreparedDocument>, String> {
        // Las media queries ya no dependen del píxel exacto: cachear por
        // baldes evita reevaluar el CSS en cada paso de un redimensionado.
        let prepared_width = prepared_width_bucket(viewport_width);
        let key = prepared_width.to_bits();
        if let Some(prepared) = self
            .prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
        {
            return Ok(prepared.clone());
        }

        let scripted = self.scripted_document()?;
        let document = Html::parse_document(&scripted.html);
        let node_styles = collect_node_styles(&document, &self.stylesheets, prepared_width);
        let mut roots = Vec::new();
        for child in document.tree.root().children() {
            if let Some(node) = convert_node(child, 0, &node_styles, Some(&self.final_url))
                .map_err(|error| error.to_string())?
            {
                roots.push(node);
            }
        }
        resolve_custom_properties(&mut roots);
        let prepared = Arc::new(PreparedDocument {
            roots,
            warnings: scripted.warnings,
        });
        let mut cache = self
            .prepared
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if cache.len() >= 16 {
            cache.clear();
        }
        cache.insert(key, prepared.clone());
        Ok(prepared)
    }

    /// Texto legible de la página para el modo lectura de Ghost: recorre el
    /// DOM ya preparado (post-script, con `display:none` respetado), une las
    /// palabras de cada bloque y separa bloques con saltos de línea. No es
    /// una extracción con heurísticas de `<article>` — es el documento tal
    /// como Wisp lo ve, sin estilos ni marcado.
    pub fn reader_text(&self) -> Option<String> {
        let prepared = self.prepared_document(READER_VIEWPORT_WIDTH).ok()?;
        let mut out = String::new();
        for root in &prepared.roots {
            collect_reader_text(root, &mut out);
            if out.len() >= MAX_READER_CHARS {
                break;
            }
        }
        // `truncate` paniquea si el corte cae dentro de un carácter UTF-8
        // (p. ej. una «á» partida por la mitad); retrocede a la frontera.
        let mut end = MAX_READER_CHARS.min(out.len());
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        let text = out.trim().to_owned();
        (!text.is_empty()).then_some(text)
    }
}

fn collect_reader_text(node: &DomNode, out: &mut String) {
    if out.len() >= MAX_READER_CHARS {
        return;
    }
    let Some(tag) = node.tag.as_deref() else {
        let text = node.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(&text);
        }
        return;
    };
    if tag == "br" {
        out.push('\n');
        return;
    }
    if inline_display(node) == Some(Display::None) {
        return;
    }
    let block = is_block(node);
    if block && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for child in &node.children {
        collect_reader_text(child, out);
    }
    if block && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Heurística de producto para el "smart engine" de Ghost: URLs con pinta de
/// documento o lectura que el pipeline de Wisp cubre bien (artículos, wikis,
/// documentación, blogs). Es conservadora — ante la duda devuelve `false` y
/// la página abre en WebView2; el usuario siempre puede forzar Wisp a mano.
/// ¿Estas advertencias significan que la página no se puede ver como en
/// Chromium? Wisp ejecuta scripts, pero no módulos ES ni scripts externos que
/// se saltan por límite o por fallo; en esos casos la página queda a medias.
pub fn warnings_need_webview(warnings: &[String]) -> bool {
    // APIs que las webs modernas asumen y que Wisp no define: si un script
    // depende de ellas la página no va a comportarse como en un navegador.
    const MISSING_JS_APIS: &[&str] = &[
        "fetch is not defined",
        "XMLHttpRequest is not defined",
        "WebSocket is not defined",
        "customElements is not defined",
        "IntersectionObserver is not defined",
        "ResizeObserver is not defined",
    ];
    warnings.iter().any(|warning| {
        warning.contains("scripts externos")
            || warning.contains("script externo")
            || warning.contains("módulo ES")
            || warning.contains("módulos ES")
            || warning.contains("módulo externo")
            || warning.contains("módulo inline")
            || warning.contains("importaciones ES")
            // Se agotó el presupuesto de JavaScript: la página quedó a medias.
            || warning.contains("límite de descarga de JavaScript")
            || warning.contains("límite de JavaScript de Wisp")
            || warning.contains("presupuesto de red")
            || warning.contains("DOM generado por JavaScript supera")
            || MISSING_JS_APIS
                .iter()
                .any(|api| warning.contains(api))
    })
}

/// Sitios que necesitan Chromium de verdad (MSE para vídeo, shadow DOM,
/// JIT) y donde Wisp nunca debe ser el motor: el intento descargaba la página
/// entera y sus recursos sólo para entregarla después a WebView2, es decir,
/// cargaba dos veces la misma página. Se eligen para WebView2 de entrada.
pub fn site_requires_chromium(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    const CHROMIUM_ONLY: &[&str] = &[
        "youtube.com",
        "youtu.be",
        "youtube-nocookie.com",
        "netflix.com",
        "twitch.tv",
        "vimeo.com",
        "instagram.com",
        "facebook.com",
        "tiktok.com",
        "x.com",
        "twitter.com",
        "spotify.com",
        "discord.com",
        "web.whatsapp.com",
        "mail.google.com",
        "docs.google.com",
        "drive.google.com",
        "maps.google.com",
        "meet.google.com",
    ];
    CHROMIUM_ONLY
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

pub fn url_suits_wisp(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let known_docs = [
        "wikipedia.org",
        "wiktionary.org",
        "wikibooks.org",
        "docs.rs",
        "developer.mozilla.org",
        "readthedocs.io",
        "rust-lang.org",
        "doc.rust-lang.org",
    ];
    if known_docs
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    {
        return true;
    }
    // Subdominios de documentación y blogs habituales.
    if ["docs.", "doc.", "wiki.", "blog."]
        .iter()
        .any(|prefix| host.starts_with(prefix))
    {
        return true;
    }
    let path = url.path().to_ascii_lowercase();
    // Solo rutas con pinta documental inequívoca: `/p/`, `/news/` y `/app/`
    // son patrones típicos de SPAs (React/Next) que pertenecen a WebView2.
    [
        "/wiki/",
        "/docs/",
        "/doc/",
        "/blog",
        "/article",
        "/post/",
        "/stories/",
        "/read/",
        ".md",
    ]
    .iter()
    .any(|marker| path.contains(marker))
}

#[derive(Debug)]
struct DomNode {
    tag: Option<String>,
    text: String,
    inline_style: String,
    href: Option<String>,
    image: Option<DomImage>,
    /// `background-image` parseado una vez al convertir el nodo (el `Style`
    /// es `Copy` y no puede portar stops de degradado).
    background_image: Option<BgImage>,
    children: Vec<DomNode>,
    /// Computed style memoized on first use: it never depends on the viewport
    /// width, so it stays valid across every resize and every Taffy measure.
    style_cache: OnceLock<Style>,
}

impl Clone for DomNode {
    fn clone(&self) -> Self {
        Self {
            tag: self.tag.clone(),
            text: self.text.clone(),
            inline_style: self.inline_style.clone(),
            href: self.href.clone(),
            image: self.image.clone(),
            background_image: self.background_image.clone(),
            children: self.children.clone(),
            style_cache: {
                let cache = OnceLock::new();
                if let Some(style) = self.style_cache.get() {
                    let _ = cache.set(*style);
                }
                cache
            },
        }
    }
}

#[derive(Clone, Debug)]
struct DomImage {
    /// Key into `FetchedPage::images`: resolved http(s) URL or full `data:` URI.
    src: String,
    alt: String,
    width: Option<f32>,
    height: Option<f32>,
}

/// Extrae la primera capa de `background-image` (o del shorthand `background`)
/// de las declaraciones ya fusionadas del nodo. `url(...)` se resuelve con la
/// misma clave que usan las `<img>`; `linear-gradient(...)` se parsea a stops.
fn node_background_image(inline_style: &str, page_url: Option<&str>) -> Option<BgImage> {
    for declaration in inline_style.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let property = property.trim();
        if !property.eq_ignore_ascii_case("background-image")
            && !property.eq_ignore_ascii_case("background")
        {
            continue;
        }
        if let Some(image) = parse_background_image(value.trim(), page_url) {
            return Some(image);
        }
    }
    None
}

fn parse_background_image(value: &str, page_url: Option<&str>) -> Option<BgImage> {
    // Solo la primera capa: las capas extra tras comas de nivel superior se
    // ignoran en esta versión.
    let layer = top_level_first_layer(value);
    if let Some(url_ref) = extract_css_function(layer, "url") {
        return resolve_image_key(page_url, &url_ref).map(BgImage::Url);
    }
    if let Some(args) = extract_css_function(layer, "linear-gradient") {
        return parse_linear_gradient(&args);
    }
    None
}

/// Devuelve el contenido entre paréntesis de la primera aparición de
/// `name(...)` (búsqueda insensible a mayúsculas), con comillas `"`/`'`
/// exterior eliminadas si las hay.
fn extract_css_function<'a>(value: &'a str, name: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let needle = format!("{name}(");
    let start = lower.find(&needle)? + needle.len();
    let mut depth = 1usize;
    let mut end = start;
    for (index, ch) in value[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = start + index;
                    break;
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    let inner = value[start..end].trim();
    let unquoted = inner
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| inner.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(inner);
    Some(unquoted.trim().to_owned())
}

/// Primera capa de una lista separada por comas, ignorando comas dentro de
/// paréntesis (p. ej. `rgba(...)` dentro de un stop de degradado).
fn top_level_first_layer(value: &str) -> &str {
    let mut depth = 0usize;
    for (index, ch) in value.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return value[..index].trim(),
            _ => {}
        }
    }
    value.trim()
}

/// Divide por comas de nivel superior (fuera de paréntesis).
fn split_top_level_commas(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, ch) in value.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(value[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(value[start..].trim());
    parts
}

/// `linear-gradient([dirección,] stop, stop, ...)`. Dirección opcional:
/// `to <lado>` o ángulo CSS (`deg`/`turn`/`rad`/`grad`). Posiciones de stop:
/// solo porcentajes; `px`/números se tratan como automáticos.
fn parse_linear_gradient(args: &str) -> Option<BgImage> {
    let parts = split_top_level_commas(args);
    if parts.len() < 2 {
        return None;
    }
    let mut index = 0usize;
    let mut angle_deg = 180.0f32; // `to bottom` por defecto
    let first = parts[0].trim();
    if first.starts_with("to ") {
        angle_deg = match first[3..].trim() {
            "top" => 0.0,
            "right" => 90.0,
            "bottom" => 180.0,
            "left" => 270.0,
            "top right" | "right top" => 45.0,
            "bottom right" | "right bottom" => 135.0,
            "bottom left" | "left bottom" => 225.0,
            "top left" | "left top" => 315.0,
            _ => 180.0,
        };
        index = 1;
    } else if let Some(angle) = parse_css_angle(first) {
        angle_deg = angle;
        index = 1;
    }
    let mut stops = Vec::new();
    for part in &parts[index..] {
        let tokens: Vec<&str> = part.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        let (color_token, position) = if tokens.len() > 1 {
            (
                tokens[..tokens.len() - 1].join(" "),
                tokens[tokens.len() - 1],
            )
        } else {
            (tokens[0].to_string(), "")
        };
        let Some(color) = parse_color(&color_token) else {
            continue;
        };
        let offset = position
            .strip_suffix('%')
            .and_then(|p| p.parse::<f32>().ok())
            .map(|p| p / 100.0)
            .unwrap_or(-1.0);
        stops.push((offset, color));
    }
    if stops.len() < 2 {
        return None;
    }
    // Posiciones automáticas: distribución uniforme salvo que el autor fije %.
    let count = stops.len();
    let resolved: Vec<GradientStop> = stops
        .iter()
        .enumerate()
        .map(|(i, (offset, color))| GradientStop {
            offset: if *offset >= 0.0 {
                offset.clamp(0.0, 1.0)
            } else {
                i as f32 / (count - 1) as f32
            },
            color: *color,
        })
        .collect();
    Some(BgImage::LinearGradient {
        angle_deg,
        stops: resolved,
    })
}

fn parse_css_angle(value: &str) -> Option<f32> {
    let value = value.trim();
    if let Some(num) = value.strip_suffix("deg") {
        return num.trim().parse::<f32>().ok();
    }
    if let Some(num) = value.strip_suffix("turn") {
        return num.trim().parse::<f32>().ok().map(|t| t * 360.0);
    }
    if let Some(num) = value.strip_suffix("rad") {
        return num.trim().parse::<f32>().ok().map(|r| r.to_degrees());
    }
    if let Some(num) = value.strip_suffix("grad") {
        return num.trim().parse::<f32>().ok().map(|g| g * 0.9);
    }
    None
}

enum InlineItem<'a> {
    Text {
        text: &'a str,
        style: Style,
        href: Option<&'a str>,
    },
    Image {
        src: String,
        width: Option<f32>,
        height: Option<f32>,
        style: Style,
        href: Option<&'a str>,
    },
    /// Salto duro de línea (`<br>`).
    Break,
}

/// Función de longitud de CSS que Wisp evalúa al maquetar. No son lineales,
/// así que se resuelven contra el ancho del contenedor en el momento del
/// layout en lugar de plegarse a `px + %` como `calc()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LengthFunction {
    Min,
    Max,
    Clamp,
}

/// Longitud CSS: píxeles absolutos, porcentaje del bloque contenedor, una
/// combinación lineal de ambos (`calc()`) o `min()`/`max()`/`clamp()` sobre
/// hasta tres argumentos.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Length {
    Px(f32),
    Percent(f32),
    Sum {
        px: f32,
        percent: f32,
    },
    /// Cada argumento se guarda como sus coeficientes lineales `(px, %)`.
    Function {
        kind: LengthFunction,
        count: u8,
        args: [[f32; 2]; 3],
    },
}

impl Default for Length {
    fn default() -> Self {
        Length::Px(0.0)
    }
}

impl Length {
    fn resolve(self, basis: f32) -> f32 {
        match self {
            Length::Px(px) => px,
            Length::Percent(percent) => basis * percent / 100.0,
            Length::Sum { px, percent } => px + basis * percent / 100.0,
            Length::Function { kind, count, args } => {
                let value = |index: usize| {
                    let [px, percent] = args[index];
                    px + basis * percent / 100.0
                };
                let count = usize::from(count.max(1));
                match kind {
                    LengthFunction::Min => (0..count).map(value).fold(f32::INFINITY, f32::min),
                    LengthFunction::Max => (0..count).map(value).fold(f32::NEG_INFINITY, f32::max),
                    // `clamp(MIN, VAL, MAX)` equivale a `max(MIN, min(VAL, MAX))`.
                    LengthFunction::Clamp => value(1).max(value(0)).min(value(2)),
                }
            }
        }
    }

    /// `true` si el valor no depende del ancho (o alto) del contenedor.
    fn is_absolute(self) -> bool {
        match self {
            Length::Px(_) => true,
            Length::Percent(_) => false,
            Length::Sum { percent, .. } => percent == 0.0,
            Length::Function { count, args, .. } => {
                (0..usize::from(count)).all(|index| args[index][1] == 0.0)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum BoxSizingKind {
    #[default]
    ContentBox,
    BorderBox,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum TextAlignKind {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum BorderStyleKind {
    #[default]
    None,
    Solid,
    Dashed,
    Dotted,
}

/// CSS `text-transform`. Se aplica al pintar, no al medir: la medida usa el
/// texto transformado para que el ajuste de línea sea el real.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum TextTransformKind {
    #[default]
    None,
    Uppercase,
    Lowercase,
    Capitalize,
}

/// CSS `white-space`: controla el colapso de espacios y los saltos duros.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum WhiteSpaceKind {
    #[default]
    Normal,
    NoWrap,
    Pre,
    PreWrap,
}

/// CSS `list-style-type`, reducido a los valores con sentido en un motor sin
/// imágenes de marcador.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum ListStyleKind {
    #[default]
    Disc,
    Circle,
    Square,
    None,
    Decimal,
    LowerAlpha,
    UpperAlpha,
    LowerRoman,
    UpperRoman,
}

/// CSS `visibility`: `hidden` reserva el espacio pero no pinta nada.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum VisibilityKind {
    #[default]
    Visible,
    Hidden,
}

/// CSS `position`. `fixed` se trata como `static` (una caja fija respecto al
/// viewport no encaja en el flujo de bloques de Wisp).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum PositionKind {
    #[default]
    Static,
    Relative,
    Absolute,
}

#[derive(Clone, Copy, Debug)]
struct Style {
    display: Display,
    color: Color,
    background: Option<Color>,
    font_size: f32,
    font_weight: u16,
    text_align: TextAlignKind,
    text_transform: TextTransformKind,
    underline: bool,
    line_through: bool,
    line_height: Option<f32>,
    white_space: WhiteSpaceKind,
    list_style: ListStyleKind,
    visibility: VisibilityKind,
    overflow_wrap: bool,
    box_sizing: BoxSizingKind,
    margin: [Length; 4],
    /// `margin: auto` por lado: el espacio libre se reparte en el eje
    /// horizontal, como el centrado clásico `margin: 0 auto`.
    margin_auto: [bool; 4],
    padding: [Length; 4],
    border_width: [Length; 4],
    border_color: [Option<Color>; 4],
    border_radius: Length,
    border_style: [BorderStyleKind; 4],
    width: Option<Length>,
    height: Option<Length>,
    min_width: Option<Length>,
    max_width: Option<Length>,
    min_height: Option<Length>,
    max_height: Option<Length>,
    position: PositionKind,
    /// `top`, `right`, `bottom`, `left`.
    insets: [Option<Length>; 4],
    flex_direction: FlexDirectionKind,
    flex_wrap: bool,
    justify_content: JustifyKind,
    align_items: AlignKind,
    gap: [Length; 2],
    flex_grow: f32,
    flex_shrink: f32,
    flex_basis: Option<Length>,
    /// Colocación explícita dentro de un grid (`grid-column`/`grid-row`).
    grid_column: GridLinePlacement,
    grid_row: GridLinePlacement,
    /// `aspect-ratio` (ancho / alto) para cajas con un solo eje definido.
    aspect_ratio: Option<f32>,
    /// `overflow` distinto de `visible`: el contenido se recorta a la caja.
    overflow_clip: bool,
}

/// Línea (o span) de un item de grid. `None` en `start`/`end` significa
/// colocación automática; `span` > 0 fija cuántas pistas ocupa el item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GridLinePlacement {
    start: Option<i16>,
    end: Option<i16>,
    span: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Display {
    Block,
    Inline,
    Flex,
    Grid,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum FlexDirectionKind {
    #[default]
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum JustifyKind {
    #[default]
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum AlignKind {
    #[default]
    Stretch,
    Start,
    End,
    Center,
    Baseline,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            display: Display::Block,
            color: Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            background: None,
            font_size: 16.0,
            font_weight: 400,
            text_align: TextAlignKind::Left,
            text_transform: TextTransformKind::None,
            underline: false,
            line_through: false,
            line_height: None,
            white_space: WhiteSpaceKind::Normal,
            list_style: ListStyleKind::Disc,
            visibility: VisibilityKind::Visible,
            overflow_wrap: false,
            box_sizing: BoxSizingKind::ContentBox,
            margin: [Length::Px(0.0); 4],
            margin_auto: [false; 4],
            padding: [Length::Px(0.0); 4],
            border_width: [Length::Px(0.0); 4],
            border_color: [None; 4],
            border_radius: Length::Px(0.0),
            border_style: [BorderStyleKind::None; 4],
            width: None,
            height: None,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            position: PositionKind::Static,
            insets: [None; 4],
            flex_direction: FlexDirectionKind::Row,
            flex_wrap: false,
            justify_content: JustifyKind::Start,
            align_items: AlignKind::Stretch,
            gap: [Length::Px(0.0); 2],
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: None,
            grid_column: GridLinePlacement::default(),
            grid_row: GridLinePlacement::default(),
            aspect_ratio: None,
            overflow_clip: false,
        }
    }
}

pub fn layout_html(html: &str, viewport_width: f32) -> Result<LayoutDocument> {
    layout_html_with_stylesheets(html, &[], viewport_width)
}

pub fn layout_html_with_stylesheets(
    html: &str,
    stylesheets: &[String],
    viewport_width: f32,
) -> Result<LayoutDocument> {
    layout_html_with_stylesheets_and_url(html, stylesheets, None, viewport_width)
}

pub fn layout_html_with_stylesheets_and_url(
    html: &str,
    stylesheets: &[String],
    page_url: Option<&str>,
    viewport_width: f32,
) -> Result<LayoutDocument> {
    layout_html_with_all_resources(
        html,
        stylesheets,
        &HashMap::new(),
        &HashMap::new(),
        page_url,
        viewport_width,
        &HashMap::new(),
    )
}

pub fn layout_html_with_resources(
    html: &str,
    stylesheets: &[String],
    external_scripts: &HashMap<String, String>,
    page_url: Option<&str>,
    viewport_width: f32,
) -> Result<LayoutDocument> {
    layout_html_with_all_resources(
        html,
        stylesheets,
        external_scripts,
        &HashMap::new(),
        page_url,
        viewport_width,
        &HashMap::new(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn layout_html_with_all_resources(
    html: &str,
    stylesheets: &[String],
    external_scripts: &HashMap<String, String>,
    module_scripts: &HashMap<String, String>,
    page_url: Option<&str>,
    viewport_width: f32,
    images: &HashMap<String, ImageResource>,
) -> Result<LayoutDocument> {
    ensure!(
        html.len() <= MAX_DOCUMENT_BYTES,
        "HTML exceeds the {MAX_DOCUMENT_BYTES}-byte Wisp prototype limit"
    );
    ensure!(
        viewport_width.is_finite() && (1.0..=16_384.0).contains(&viewport_width),
        "viewport width must be between 1 and 16384 CSS pixels"
    );

    let document = Html::parse_document(html);
    let script_result = script::execute_scripts(
        &document,
        external_scripts,
        module_scripts,
        page_url,
        viewport_width,
    )?;
    let final_html = script_result.html.unwrap_or_else(|| html.to_owned());
    let mut layout =
        layout_prepared_document(&final_html, stylesheets, page_url, viewport_width, images)?;
    layout.warnings = script_result
        .warnings
        .into_iter()
        .chain(layout.warnings)
        .collect();
    Ok(layout)
}

/// Parse → CSS → layout for markup whose scripts already ran (or never
/// existed). This is the cheap stage that [`FetchedPage::layout`] repeats on
/// every viewport resize.
fn layout_prepared_document(
    html: &str,
    stylesheets: &[String],
    page_url: Option<&str>,
    viewport_width: f32,
    images: &HashMap<String, ImageResource>,
) -> Result<LayoutDocument> {
    let document = Html::parse_document(html);
    let node_styles = collect_node_styles(&document, stylesheets, viewport_width);
    let mut roots = Vec::new();
    for child in document.tree.root().children() {
        if let Some(node) = convert_node(child, 0, &node_styles, page_url)? {
            roots.push(node);
        }
    }
    resolve_custom_properties(&mut roots);
    layout_roots(&roots, viewport_width, images)
}

/// Block layout over already-converted DOM roots. This is the only stage
/// that depends on the viewport width, so [`FetchedPage::layout`] can repeat
/// it cheaply on every resize.
fn layout_roots(
    roots: &[DomNode],
    viewport_width: f32,
    images: &HashMap<String, ImageResource>,
) -> Result<LayoutDocument> {
    let mut commands = vec![PaintCommand::FillRect {
        rect: Rect {
            x: 0.0,
            y: 0.0,
            width: viewport_width,
            height: 0.0,
        },
        color: DEFAULT_COLOR,
        radius: 0.0,
    }];
    let mut cursor_y = 0.0;
    let mut measure_cache = MeasureCache::new();
    for root in roots {
        if is_block(root) {
            cursor_y += layout_block(
                root,
                0.0,
                cursor_y,
                viewport_width,
                Style::default(),
                images,
                &mut commands,
                0,
                false,
                &mut measure_cache,
            )?;
        }
    }

    if let Some(PaintCommand::FillRect { rect, .. }) = commands.first_mut() {
        rect.height = cursor_y.max(1.0);
    }

    Ok(LayoutDocument {
        viewport_width,
        content_height: cursor_y,
        commands,
        warnings: Vec::new(),
    })
}

#[derive(Default)]
struct NetworkBudgetState {
    deadline: Option<Instant>,
    skipped: usize,
}

thread_local! {
    /// Presupuesto de red de la página que se descarga en este hilo. `fetch_url`
    /// corre en un hilo por carga, así que el thread-local acota exactamente
    /// una página aunque el usuario abra varias a la vez.
    static NETWORK_BUDGET: RefCell<NetworkBudgetState> = RefCell::new(NetworkBudgetState::default());
}

fn begin_network_budget() {
    NETWORK_BUDGET.with(|budget| {
        let mut budget = budget.borrow_mut();
        budget.deadline = Some(Instant::now() + MAX_NETWORK_TIME);
        budget.skipped = 0;
    });
}

/// Tiempo máximo que puede durar la siguiente petición. `None` cuando el
/// presupuesto de la página ya se agotó: el recurso se omite y se cuenta para
/// el aviso final.
fn network_request_timeout(cap: Duration) -> Option<Duration> {
    NETWORK_BUDGET.with(|budget| {
        let mut budget = budget.borrow_mut();
        let Some(deadline) = budget.deadline else {
            return Some(cap);
        };
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            budget.skipped += 1;
            return None;
        }
        Some(cap.min(left))
    })
}

/// Aviso de recursos omitidos por el presupuesto de red, si hubo alguno.
fn take_network_warning() -> Option<String> {
    NETWORK_BUDGET.with(|budget| {
        let mut budget = budget.borrow_mut();
        let skipped = std::mem::take(&mut budget.skipped);
        (skipped > 0).then(|| {
            format!(
                "Se alcanzó el presupuesto de red de {} s: {skipped} recursos no se descargaron.",
                MAX_NETWORK_TIME.as_secs()
            )
        })
    })
}

pub fn fetch_url(url: &str) -> Result<FetchedPage> {
    begin_network_budget();
    let url = url::Url::parse(url)?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "Wisp Beta only supports HTTP and HTTPS pages"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "URLs containing embedded credentials are not supported"
    );

    let client = Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(Policy::limited(10))
        .user_agent("GhostWisp/0.1")
        .build()?;
    let response = client
        .get(url)
        .header(ACCEPT, "text/html,application/xhtml+xml")
        .send()?;
    ensure!(
        response.status().is_success(),
        "Wisp received HTTP status {}",
        response.status()
    );
    if let Some(content_type) = response.headers().get(CONTENT_TYPE) {
        let mime = content_type
            .to_str()?
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        ensure!(
            mime == "text/html" || mime == "application/xhtml+xml",
            "Wisp Beta can only render HTML; the server returned {mime}"
        );
    }
    if let Some(length) = response.content_length() {
        ensure!(
            length <= MAX_DOCUMENT_BYTES as u64,
            "HTML response exceeds Wisp's {MAX_DOCUMENT_BYTES}-byte limit"
        );
    }

    let final_url = response.url().to_string();
    let mut body = Vec::new();
    response
        .take((MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    ensure!(
        body.len() <= MAX_DOCUMENT_BYTES,
        "HTML response exceeds Wisp's {MAX_DOCUMENT_BYTES}-byte limit"
    );
    let html = String::from_utf8_lossy(&body).into_owned();
    let title = title_from_html(&html)?;
    let (stylesheets, mut warnings) = fetch_stylesheets(&client, &html, &final_url);
    let (external_scripts, script_warnings) = fetch_external_scripts(&client, &html, &final_url);
    warnings.extend(script_warnings);
    let (module_scripts, module_warnings) = fetch_module_scripts(&client, &html, &final_url);
    warnings.extend(module_warnings);
    let (mut images, image_warnings) = fetch_images(&client, &html, &final_url);
    warnings.extend(image_warnings);
    fetch_background_images(&client, &html, &stylesheets, &final_url, &mut images);
    if let Some(warning) = take_network_warning() {
        warnings.push(warning);
    }

    Ok(FetchedPage::new(
        final_url,
        title,
        html,
        stylesheets,
        external_scripts,
        module_scripts,
        images,
        warnings,
    ))
}

fn fetch_stylesheets(client: &Client, html: &str, base_url: &str) -> (Vec<String>, Vec<String>) {
    let mut stylesheets = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut total_bytes = 0usize;
    let mut attempted = 0usize;
    let mut imported = 0usize;
    let stylesheet_selector = match Selector::parse("link[rel][href]") {
        Ok(selector) => selector,
        Err(error) => {
            return (
                stylesheets,
                vec![format!("No se pudieron analizar los enlaces CSS: {error}")],
            );
        }
    };
    let document = Html::parse_document(html);
    let Ok(base_url) = url::Url::parse(base_url) else {
        return (
            stylesheets,
            vec!["La URL base de la página no es válida.".into()],
        );
    };

    for link in document.select(&stylesheet_selector) {
        let is_stylesheet = link.attr("rel").is_some_and(|rel| {
            rel.split_ascii_whitespace()
                .any(|token| token.eq_ignore_ascii_case("stylesheet"))
        });
        if !is_stylesheet {
            continue;
        }
        let Some(href) = link.attr("href") else {
            continue;
        };
        if attempted >= MAX_STYLESHEET_COUNT {
            warnings.push(format!(
                "Se omitieron hojas CSS después del límite de {MAX_STYLESHEET_COUNT}."
            ));
            break;
        }
        attempted += 1;

        let stylesheet_url = match base_url.join(href) {
            Ok(url)
                if matches!(url.scheme(), "http" | "https")
                    && url.username().is_empty()
                    && url.password().is_none() =>
            {
                url
            }
            _ => {
                warnings.push(format!("Se omitió una URL CSS no válida: {href}"));
                continue;
            }
        };
        let stylesheet_url = stylesheet_url.to_string();
        if !seen.insert(stylesheet_url.clone()) {
            continue;
        }

        fetch_stylesheet_tree(
            client,
            &stylesheet_url,
            &mut total_bytes,
            &mut imported,
            &mut seen,
            &mut stylesheets,
            &mut warnings,
            0,
        );
    }

    (stylesheets, warnings)
}

/// Descarga una hoja CSS, resuelve sus `@import` (antes que sus propias
/// reglas, como manda la cascada) y añade el texto a `stylesheets`.
#[allow(clippy::too_many_arguments)]
fn fetch_stylesheet_tree(
    client: &Client,
    stylesheet_url: &str,
    total_bytes: &mut usize,
    imported: &mut usize,
    seen: &mut HashSet<String>,
    stylesheets: &mut Vec<String>,
    warnings: &mut Vec<String>,
    depth: usize,
) {
    let Some(timeout) = network_request_timeout(MAX_NETWORK_REQUEST_TIMEOUT) else {
        return;
    };
    let response = match client.get(stylesheet_url).timeout(timeout).send() {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            warnings.push(format!(
                "No se pudo cargar una hoja CSS (HTTP {}).",
                response.status()
            ));
            return;
        }
        Err(error) => {
            warnings.push(format!("No se pudo cargar una hoja CSS: {error}"));
            return;
        }
    };

    if let Some(content_type) = response.headers().get(CONTENT_TYPE) {
        let mime = content_type
            .to_str()
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !matches!(mime.as_str(), "text/css" | "text/plain") {
            warnings.push(format!(
                "Se omitió un recurso CSS con tipo de contenido {mime}."
            ));
            return;
        }
    }

    let remaining_bytes = MAX_STYLESHEET_BYTES.saturating_sub(*total_bytes);
    if remaining_bytes == 0
        || response
            .content_length()
            .is_some_and(|length| length > remaining_bytes as u64)
    {
        warnings.push("Se alcanzó el límite de descarga de CSS de Wisp.".into());
        return;
    }
    let mut body = Vec::new();
    match response
        .take((remaining_bytes + 1) as u64)
        .read_to_end(&mut body)
    {
        Ok(_) if body.len() <= remaining_bytes => {
            *total_bytes += body.len();
        }
        Ok(_) => {
            warnings.push("Se alcanzó el límite de descarga de CSS de Wisp.".into());
            return;
        }
        Err(error) => {
            warnings.push(format!("No se pudo leer una hoja CSS: {error}"));
            return;
        }
    }

    let css = String::from_utf8_lossy(&body).into_owned();
    if depth < MAX_IMPORT_DEPTH
        && let Ok(base) = url::Url::parse(stylesheet_url)
    {
        for import in stylesheet_import_urls(&css) {
            if *imported >= MAX_IMPORT_COUNT {
                warnings.push(format!(
                    "Se omitieron @import después del límite de {MAX_IMPORT_COUNT}."
                ));
                break;
            }
            let Ok(target) = base.join(&import) else {
                continue;
            };
            if !matches!(target.scheme(), "http" | "https")
                || !target.username().is_empty()
                || target.password().is_some()
            {
                continue;
            }
            let target = target.to_string();
            if !seen.insert(target.clone()) {
                continue;
            }
            *imported += 1;
            fetch_stylesheet_tree(
                client,
                &target,
                total_bytes,
                imported,
                seen,
                stylesheets,
                warnings,
                depth + 1,
            );
        }
    }
    stylesheets.push(css);
}

/// URLs de las reglas `@import` de una hoja de estilos, en orden.
fn stylesheet_import_urls(stylesheet: &str) -> Vec<String> {
    let bytes = stylesheet.as_bytes();
    let lowercase = stylesheet.to_ascii_lowercase();
    let mut urls = Vec::new();
    let mut index = 0usize;
    while let Some(offset) = lowercase[index..].find("@import") {
        let at = index + offset;
        let mut cursor = at + "@import".len();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if let Some((url, next)) = read_css_url(stylesheet, cursor) {
            urls.push(url);
            index = next;
        } else {
            index = cursor + 1;
        }
    }
    urls
}

/// Lee una URL de CSS: `url("...")`, `url(...)`, `"..."` o `'...'`.
fn is_css_quote(byte: u8) -> bool {
    byte == b'"' || byte == b'\''
}

fn read_css_url(stylesheet: &str, start: usize) -> Option<(String, usize)> {
    let bytes = stylesheet.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    if bytes[start..]
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"url("))
    {
        let mut cursor = start + 4;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let quote = bytes
            .get(cursor)
            .copied()
            .filter(|byte| is_css_quote(*byte));
        if quote.is_some() {
            cursor += 1;
        }
        let content_start = cursor;
        while cursor < bytes.len() {
            match quote {
                Some(quote) if bytes[cursor] == quote => break,
                None if bytes[cursor] == b')' => break,
                _ => cursor += 1,
            }
        }
        let url = stylesheet[content_start..cursor].trim().to_owned();
        return (!url.is_empty()).then_some((url, cursor + 1));
    }
    let quote = *bytes.get(start)?;
    if !is_css_quote(quote) {
        return None;
    }
    let content_start = start + 1;
    let mut cursor = content_start;
    while cursor < bytes.len() && bytes[cursor] != quote {
        cursor += 1;
    }
    let url = stylesheet[content_start..cursor].trim().to_owned();
    (!url.is_empty()).then_some((url, cursor + 1))
}

fn fetch_external_scripts(
    client: &Client,
    html: &str,
    base_url: &str,
) -> (HashMap<String, String>, Vec<String>) {
    let mut scripts = HashMap::new();
    let mut warnings = Vec::new();
    let mut total_bytes = 0usize;
    let mut attempted = 0usize;
    let script_selector = match Selector::parse("script[src]") {
        Ok(selector) => selector,
        Err(error) => {
            return (
                scripts,
                vec![format!("No se pudieron analizar los scripts: {error}")],
            );
        }
    };
    let document = Html::parse_document(html);
    let Ok(base_url) = url::Url::parse(base_url) else {
        return (
            scripts,
            vec!["La URL base de la página no es válida.".into()],
        );
    };

    for script in document.select(&script_selector) {
        if script.attr("nomodule").is_some() {
            continue;
        }
        let script_type = script
            .attr("type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if matches!(
            script_type.as_str(),
            "module" | "application/json" | "application/ld+json" | "importmap"
        ) || !matches!(
            script_type.as_str(),
            "" | "text/javascript"
                | "application/javascript"
                | "text/ecmascript"
                | "application/ecmascript"
        ) {
            continue;
        }
        if attempted >= MAX_EXTERNAL_SCRIPT_COUNT {
            warnings.push(format!(
                "Se omitieron scripts externos después del límite de {MAX_EXTERNAL_SCRIPT_COUNT}."
            ));
            break;
        }

        let Some(src) = script.attr("src") else {
            continue;
        };
        let script_url = match base_url.join(src) {
            Ok(url)
                if matches!(url.scheme(), "http" | "https")
                    && url.username().is_empty()
                    && url.password().is_none() =>
            {
                url
            }
            _ => {
                warnings.push(format!("Se omitió una URL de script no válida: {src}"));
                continue;
            }
        };
        let script_url = script_url.to_string();
        if scripts.contains_key(&script_url) {
            continue;
        }
        attempted += 1;

        let Some(timeout) = network_request_timeout(MAX_NETWORK_REQUEST_TIMEOUT) else {
            break;
        };
        let response = match client.get(&script_url).timeout(timeout).send() {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                warnings.push(format!(
                    "No se pudo cargar un script (HTTP {}).",
                    response.status()
                ));
                continue;
            }
            Err(error) => {
                warnings.push(format!("No se pudo cargar un script: {error}"));
                continue;
            }
        };

        if let Some(content_type) = response.headers().get(CONTENT_TYPE) {
            let mime = content_type
                .to_str()
                .unwrap_or_default()
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if !matches!(
                mime.as_str(),
                "text/javascript"
                    | "application/javascript"
                    | "application/x-javascript"
                    | "text/ecmascript"
                    | "application/ecmascript"
                    | "text/plain"
                    | "application/octet-stream"
            ) {
                warnings.push(format!("Se omitió un script con tipo de contenido {mime}."));
                continue;
            }
        }

        let remaining_bytes = MAX_TOTAL_EXTERNAL_SCRIPT_BYTES.saturating_sub(total_bytes);
        if remaining_bytes == 0 {
            warnings.push("Se alcanzó el límite de descarga de JavaScript de Wisp.".into());
            break;
        }
        if response
            .content_length()
            .is_some_and(|length| length > remaining_bytes as u64)
        {
            warnings.push("Se alcanzó el límite de descarga de JavaScript de Wisp.".into());
            break;
        }
        let read_limit = remaining_bytes.min(MAX_EXTERNAL_SCRIPT_BYTES + 1);
        let mut body = Vec::new();
        match response.take(read_limit as u64).read_to_end(&mut body) {
            Ok(_) if body.len() > MAX_EXTERNAL_SCRIPT_BYTES => {
                warnings.push(format!(
                    "Se omitió un script externo por superar el límite de {} KiB.",
                    MAX_EXTERNAL_SCRIPT_BYTES / 1024
                ));
                continue;
            }
            Ok(_) => {
                total_bytes += body.len();
                scripts.insert(script_url, String::from_utf8_lossy(&body).into_owned());
            }
            Err(error) => {
                warnings.push(format!("No se pudo leer un script: {error}"));
            }
        }
    }

    (scripts, warnings)
}

/// Fetches the transitive graph of classic `type="module"` scripts.
///
/// Wisp does not run a network stack inside the JavaScript engine, so the
/// whole statically discoverable graph is downloaded up front with the same
/// per-resource and aggregate limits used for classic scripts.
fn fetch_module_scripts(
    client: &Client,
    html: &str,
    base_url: &str,
) -> (HashMap<String, String>, Vec<String>) {
    let mut scripts = HashMap::new();
    let mut warnings = Vec::new();
    let Ok(base) = url::Url::parse(base_url) else {
        return (
            scripts,
            vec!["La URL base de la página no es válida.".into()],
        );
    };
    let script_selector = match Selector::parse("script") {
        Ok(selector) => selector,
        Err(error) => {
            return (
                scripts,
                vec![format!("No se pudieron analizar los módulos: {error}")],
            );
        }
    };

    let document = Html::parse_document(html);
    let mut queue: VecDeque<(String, Option<String>, usize)> = VecDeque::new();
    let mut inline_index = 0usize;
    for script in document.select(&script_selector) {
        let script_type = script
            .attr("type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if script_type != "module" {
            continue;
        }
        if let Some(src) = script.attr("src") {
            let Some(url) = resolve_resource_url(&base, src) else {
                warnings.push(format!("Se omitió una URL de módulo no válida: {src}"));
                continue;
            };
            queue.push_back((url, None, 0));
        } else {
            let source = script.text().collect::<String>();
            let key = format!("{}#wisp-inline-module-{inline_index}", base.as_str());
            inline_index += 1;
            queue.push_back((key, Some(source), 0));
        }
    }

    let mut seen = HashSet::new();
    let mut total_bytes = 0usize;
    let mut unresolved = 0usize;
    let mut exhausted = false;

    while let Some((url, source, depth)) = queue.pop_front() {
        if !seen.insert(url.clone()) {
            continue;
        }
        if seen.len() > MAX_MODULE_COUNT {
            warnings.push(format!(
                "Se alcanzó el límite de {MAX_MODULE_COUNT} módulos ES en una página."
            ));
            break;
        }
        let source = match source {
            Some(source) => source,
            None => match fetch_javascript_source(client, &url) {
                Ok(source) => source,
                Err(warning) => {
                    warnings.push(warning);
                    continue;
                }
            },
        };
        if source.len() > MAX_MODULE_BYTES {
            warnings.push(format!(
                "Se omitió un módulo por superar el límite de {} KiB.",
                MAX_MODULE_BYTES / 1024
            ));
            continue;
        }
        total_bytes = total_bytes.saturating_add(source.len());
        if total_bytes > MAX_TOTAL_MODULE_BYTES {
            warnings.push("Se alcanzó el límite de descarga de módulos ES de Wisp.".into());
            exhausted = true;
            break;
        }

        for specifier in module_specifiers(&source) {
            let Some(dependency) = resolve_module_url(&url, &specifier) else {
                unresolved += 1;
                continue;
            };
            if depth + 1 > MAX_MODULE_DEPTH {
                warnings.push(format!(
                    "Se omitieron importaciones que superan la profundidad de {MAX_MODULE_DEPTH} módulos."
                ));
                continue;
            }
            if !seen.contains(&dependency) {
                queue.push_back((dependency, None, depth + 1));
            }
        }
        scripts.insert(url, source);
    }

    if unresolved > 0 {
        warnings.push(format!(
            "Wisp no pudo resolver {unresolved} importaciones ES (por ejemplo, paquetes sin URL)."
        ));
    }
    if !exhausted && scripts.len() >= MAX_MODULE_COUNT {
        warnings.push(format!(
            "Se alcanzó el límite de {MAX_MODULE_COUNT} módulos ES en una página."
        ));
    }

    (scripts, warnings)
}

/// Fetches the images referenced by `<img src>` so layout can reserve space
/// and the UI can paint them. `data:` URIs are decoded in place; remote URLs
/// go through the same http(s)-only, no-credentials checks as other resources.
fn fetch_images(
    client: &Client,
    html: &str,
    base_url: &str,
) -> (HashMap<String, ImageResource>, Vec<String>) {
    let mut images = HashMap::new();
    let mut warnings = Vec::new();
    let mut seen = HashSet::new();
    let mut total_bytes = 0usize;
    let image_selector = match Selector::parse("img") {
        Ok(selector) => selector,
        Err(error) => {
            return (
                images,
                vec![format!("No se pudieron analizar las imágenes: {error}")],
            );
        }
    };
    let document = Html::parse_document(html);
    let Ok(base_url) = url::Url::parse(base_url) else {
        return (
            images,
            vec!["La URL base de la página no es válida.".into()],
        );
    };

    for image in document.select(&image_selector) {
        // `<img srcset>` sin `src` es habitual (carga diferida, arte
        // responsive): se usa el candidato de mayor descriptor.
        let src = image
            .attr("src")
            .map(str::trim)
            .filter(|src| !src.is_empty())
            .or_else(|| image.attr("srcset").and_then(largest_srcset_candidate))
            .unwrap_or_default();
        if src.is_empty() {
            continue;
        }

        let (key, data) = if let Some(data) = src.strip_prefix("data:") {
            let Some(bytes) = decode_data_image(data) else {
                continue;
            };
            (src.to_owned(), bytes)
        } else {
            let Ok(url) = base_url.join(src) else {
                warnings.push(format!("Se omitió una URL de imagen no válida: {src}"));
                continue;
            };
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                warnings.push(format!("Se omitió una URL de imagen no válida: {src}"));
                continue;
            }
            let key = url.to_string();
            if images.contains_key(&key) || seen.contains(&key) {
                continue;
            }
            if seen.len() >= MAX_IMAGE_COUNT {
                warnings.push(format!(
                    "Se alcanzó el límite de {MAX_IMAGE_COUNT} imágenes por página."
                ));
                break;
            }
            seen.insert(key.clone());

            let Some(timeout) = network_request_timeout(Duration::from_secs(10)) else {
                break;
            };
            let response = match client.get(&key).timeout(timeout).send() {
                Ok(response) if response.status().is_success() => response,
                Ok(response) => {
                    warnings.push(format!(
                        "No se pudo cargar una imagen (HTTP {}).",
                        response.status()
                    ));
                    continue;
                }
                Err(error) => {
                    warnings.push(format!("No se pudo cargar una imagen: {error}"));
                    continue;
                }
            };
            if let Some(content_type) = response.headers().get(CONTENT_TYPE) {
                let mime = content_type
                    .to_str()
                    .unwrap_or_default()
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                if !(mime.starts_with("image/")
                    || mime == "application/octet-stream"
                    || mime.is_empty())
                {
                    warnings.push(format!(
                        "Se omitió una imagen con tipo de contenido {mime}."
                    ));
                    continue;
                }
            }
            let remaining_bytes = MAX_TOTAL_IMAGE_BYTES.saturating_sub(total_bytes);
            if remaining_bytes == 0 {
                warnings.push("Se alcanzó el límite de descarga de imágenes de Wisp.".into());
                break;
            }
            let read_limit = remaining_bytes.min(MAX_IMAGE_BYTES + 1);
            let mut body = Vec::new();
            match response.take(read_limit as u64).read_to_end(&mut body) {
                Ok(_) if body.len() > MAX_IMAGE_BYTES => {
                    warnings.push(format!(
                        "Se omitió una imagen por superar el límite de {} KiB.",
                        MAX_IMAGE_BYTES / 1024
                    ));
                    continue;
                }
                Ok(_) => {
                    total_bytes += body.len();
                    (key, body)
                }
                Err(error) => {
                    warnings.push(format!("No se pudo leer una imagen: {error}"));
                    continue;
                }
            }
        };

        if data.len() > MAX_IMAGE_BYTES {
            warnings.push(format!(
                "Se omitió una imagen por superar el límite de {} KiB.",
                MAX_IMAGE_BYTES / 1024
            ));
            continue;
        }
        total_bytes = total_bytes.saturating_add(data.len());
        if total_bytes > MAX_TOTAL_IMAGE_BYTES {
            warnings.push("Se alcanzó el límite de descarga de imágenes de Wisp.".into());
            break;
        }
        let (width, height) = imagesize::blob_size(&data)
            .map(|size| (size.width as u32, size.height as u32))
            .unwrap_or((0, 0));
        images.insert(
            key,
            ImageResource {
                data,
                width,
                height,
            },
        );
    }

    (images, warnings)
}

/// Referencias `url(...)` que aparecen en declaraciones `background` /
/// `background-image`, tanto en atributos `style`, bloques `<style>` del HTML
/// como en las hojas de estilo externas ya descargadas.
fn collect_background_urls<'a>(texts: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    for text in texts {
        let lower = text.to_ascii_lowercase();
        let mut search = 0usize;
        while let Some(relative) = lower.get(search..).and_then(|s| s.find("url(")) {
            let pos = search + relative;
            let segment_start = text[..pos]
                .rfind(|c| c == ';' || c == '{' || c == '}')
                .map(|index| index + 1)
                .unwrap_or(0);
            if text[segment_start..pos].contains("background")
                && let Some(reference) = extract_css_function(text.get(pos..).unwrap_or(""), "url")
                && seen.insert(reference.clone())
            {
                urls.push(reference);
            }
            search = pos + 4;
        }
    }
    urls
}

/// Descarga las imágenes de `background-image`/`background` y las guarda en el
/// mismo mapa que las `<img>` (misma clave resuelta). Comparte los límites de
/// `fetch_images`; un fondo que falle no genera warning para no ensuciar la UI.
fn fetch_background_images(
    client: &Client,
    html: &str,
    stylesheets: &[String],
    base_url: &str,
    images: &mut HashMap<String, ImageResource>,
) {
    let Ok(base_url) = url::Url::parse(base_url) else {
        return;
    };
    let mut seen: HashSet<String> = images.keys().cloned().collect();
    let mut total_bytes = 0usize;
    let mut fetched = 0usize;
    let sources = std::iter::once(html).chain(stylesheets.iter().map(String::as_str));
    for reference in collect_background_urls(sources) {
        if fetched >= MAX_IMAGE_COUNT {
            break;
        }
        let (key, data) = if let Some(payload) = reference.strip_prefix("data:") {
            let Some(bytes) = decode_data_image(payload) else {
                continue;
            };
            (reference.clone(), bytes)
        } else {
            let Ok(url) = base_url.join(&reference) else {
                continue;
            };
            if !matches!(url.scheme(), "http" | "https")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                continue;
            }
            let key = url.to_string();
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key.clone());
            fetched += 1;
            let Some(timeout) = network_request_timeout(Duration::from_secs(10)) else {
                break;
            };
            let Ok(response) = client.get(&key).timeout(timeout).send() else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            if let Some(content_type) = response.headers().get(CONTENT_TYPE) {
                let mime = content_type
                    .to_str()
                    .unwrap_or_default()
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                if !mime.starts_with("image/") {
                    continue;
                }
            }
            let Ok(bytes) = response.bytes() else {
                continue;
            };
            (key, bytes.to_vec())
        };
        if data.len() > MAX_IMAGE_BYTES {
            continue;
        }
        total_bytes = total_bytes.saturating_add(data.len());
        if total_bytes > MAX_TOTAL_IMAGE_BYTES {
            break;
        }
        let (width, height) = imagesize::blob_size(&data)
            .map(|size| (size.width as u32, size.height as u32))
            .unwrap_or((0, 0));
        images.insert(
            key,
            ImageResource {
                data,
                width,
                height,
            },
        );
    }
}

/// Decodes `data:image/...;base64,...` payloads. Only base64-encoded image
/// payloads are supported; SVG and other text formats are skipped.
fn decode_data_image(payload: &str) -> Option<Vec<u8>> {
    let (metadata, data) = payload.split_once(',')?;
    let mime = metadata.split(';').next().unwrap_or_default().trim();
    if !mime.to_ascii_lowercase().starts_with("image/") {
        return None;
    }
    let is_base64 = metadata
        .split(';')
        .skip(1)
        .any(|part| part.trim().eq_ignore_ascii_case("base64"));
    if !is_base64 {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()
}

/// Resolves an `<img src>` to the same key used by [`fetch_images`]: the
/// absolute http(s) URL, or the full `data:` URI.
/// Candidato de mayor descriptor de un `srcset` (`url 480w`, `url 2x`).
fn largest_srcset_candidate(srcset: &str) -> Option<&str> {
    let mut best: Option<(f32, &str)> = None;
    let mut first_without_descriptor: Option<&str> = None;
    for candidate in split_top_level_commas(srcset) {
        let mut parts = candidate.split_whitespace();
        let Some(url) = parts.next() else {
            continue;
        };
        let descriptor = parts.next().unwrap_or_default();
        if descriptor.is_empty() {
            first_without_descriptor.get_or_insert(url);
            continue;
        }
        let weight = if let Some(width) = descriptor.strip_suffix('w') {
            width.parse::<f32>().ok()
        } else if let Some(density) = descriptor.strip_suffix('x') {
            density.parse::<f32>().ok().map(|density| density * 1000.0)
        } else {
            None
        };
        if let Some(weight) = weight
            && best.is_none_or(|(best_weight, _)| weight > best_weight)
        {
            best = Some((weight, url));
        }
    }
    best.map(|(_, url)| url).or(first_without_descriptor)
}

fn resolve_image_key(page_url: Option<&str>, src: &str) -> Option<String> {
    let src = src.trim();
    if src.is_empty() {
        return None;
    }
    if src.starts_with("data:") {
        return Some(src.to_owned());
    }
    if let Ok(url) = url::Url::parse(src) {
        return web_resource_url(&url);
    }
    let base = url::Url::parse(page_url?).ok()?;
    let url = base.join(src).ok()?;
    web_resource_url(&url)
}

fn web_resource_url(url: &url::Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.to_string())
}

fn resolve_resource_url(base: &url::Url, reference: &str) -> Option<String> {
    let url = base.join(reference).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(url.to_string())
}

fn resolve_module_url(base_url: &str, specifier: &str) -> Option<String> {
    if !is_url_like_module_specifier(specifier) {
        return None;
    }
    let base = url::Url::parse(base_url).ok()?;
    let url = base.join(specifier).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    Some(url.to_string())
}

fn is_url_like_module_specifier(specifier: &str) -> bool {
    specifier.starts_with('/')
        || specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.contains("://")
        || specifier.starts_with("data:")
        || specifier.starts_with("blob:")
}

fn fetch_javascript_source(client: &Client, script_url: &str) -> Result<String, String> {
    let timeout = network_request_timeout(MAX_NETWORK_REQUEST_TIMEOUT).ok_or_else(|| {
        format!(
            "Se alcanzó el presupuesto de red de {} s: un módulo no se descargó.",
            MAX_NETWORK_TIME.as_secs()
        )
    })?;
    let response = client
        .get(script_url)
        .timeout(timeout)
        .send()
        .map_err(|error| format!("No se pudo cargar un módulo: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "No se pudo cargar un módulo (HTTP {}).",
            response.status()
        ));
    }
    if let Some(content_type) = response.headers().get(CONTENT_TYPE) {
        let mime = content_type
            .to_str()
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            mime.as_str(),
            "text/javascript"
                | "application/javascript"
                | "application/x-javascript"
                | "text/ecmascript"
                | "application/ecmascript"
                | "text/plain"
                | "application/octet-stream"
        ) {
            return Err(format!("Se omitió un módulo con tipo de contenido {mime}."));
        }
    }
    let mut body = Vec::new();
    response
        .take((MAX_MODULE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("No se pudo leer un módulo: {error}"))?;
    if body.len() > MAX_MODULE_BYTES {
        return Err(format!(
            "Se omitió un módulo por superar el límite de {} KiB.",
            MAX_MODULE_BYTES / 1024
        ));
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Extracts static `import`/`export ... from` specifiers and literal dynamic
/// imports from JavaScript source without a full parser. Comments, strings and
/// template literals are skipped so text that only looks like an import is not
/// treated as one.
fn module_specifiers(source: &str) -> Vec<String> {
    let mut specifiers = Vec::new();
    let bytes = source.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'\'' | b'"' => {
                index = skip_js_string(bytes, index);
            }
            b'`' => {
                index = skip_js_template(bytes, index);
            }
            byte if is_identifier_start(byte) => {
                let start = index;
                index += 1;
                while index < bytes.len() && is_identifier_continue(bytes[index]) {
                    index += 1;
                }
                match &source[start..index] {
                    "import" => {
                        if let Some((specifier, next)) = import_specifier(bytes, index) {
                            push_specifier(&mut specifiers, &specifier);
                            index = next;
                        }
                    }
                    "export" => {
                        if let Some((specifier, next)) = export_specifier(bytes, index) {
                            push_specifier(&mut specifiers, &specifier);
                            index = next;
                        }
                    }
                    _ => {}
                }
            }
            _ => index += 1,
        }
    }
    specifiers
}

fn push_specifier(specifiers: &mut Vec<String>, specifier: &str) {
    if !specifier.is_empty() && !specifiers.iter().any(|item| item == specifier) {
        specifiers.push(specifier.to_owned());
    }
}

fn import_specifier(bytes: &[u8], mut index: usize) -> Option<(String, usize)> {
    index = skip_js_whitespace(bytes, index);
    if index >= bytes.len() {
        return None;
    }
    if bytes[index] == b'.' {
        // `import.meta`.
        return None;
    }
    if bytes[index] == b'(' {
        index = skip_js_whitespace(bytes, index + 1);
        if bytes.get(index) == Some(&b'`') {
            return read_js_template_literal(bytes, index);
        }
        let (specifier, next) = read_js_string(bytes, index)?;
        return Some((specifier, next));
    }
    if bytes[index] == b'\'' || bytes[index] == b'"' {
        return read_js_string(bytes, index);
    }

    let mut cursor = index;
    let limit = (index + 4096).min(bytes.len());
    while cursor < limit {
        match bytes[cursor] {
            b'\'' | b'"' => {
                cursor = skip_js_string(bytes, cursor);
            }
            b'`' => {
                cursor = skip_js_template(bytes, cursor);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                while cursor < limit && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor += 2;
                while cursor + 1 < limit && !(bytes[cursor] == b'*' && bytes[cursor + 1] == b'/') {
                    cursor += 1;
                }
                cursor = (cursor + 2).min(limit);
            }
            b';' => return None,
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < limit && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if &bytes[start..cursor] == b"from" {
                    let next = skip_js_whitespace(bytes, cursor);
                    if next < bytes.len() {
                        if let Some((specifier, next)) = read_js_string(bytes, next) {
                            return Some((specifier, next));
                        }
                    }
                }
            }
            _ => cursor += 1,
        }
    }
    None
}

fn export_specifier(bytes: &[u8], index: usize) -> Option<(String, usize)> {
    let limit = (index + 4096).min(bytes.len());
    let mut cursor = index;
    while cursor < limit {
        match bytes[cursor] {
            b';' | b'\n' => return None,
            b'\'' | b'"' => {
                cursor = skip_js_string(bytes, cursor);
            }
            b'`' => {
                cursor = skip_js_template(bytes, cursor);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => return None,
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor += 2;
                while cursor + 1 < limit && !(bytes[cursor] == b'*' && bytes[cursor + 1] == b'/') {
                    cursor += 1;
                }
                cursor = (cursor + 2).min(limit);
            }
            byte if is_identifier_start(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < limit && is_identifier_continue(bytes[cursor]) {
                    cursor += 1;
                }
                if &bytes[start..cursor] == b"from" {
                    let next = skip_js_whitespace(bytes, cursor);
                    if next < bytes.len() {
                        if let Some((specifier, next)) = read_js_string(bytes, next) {
                            return Some((specifier, next));
                        }
                    }
                }
            }
            _ => cursor += 1,
        }
    }
    None
}

fn skip_js_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn skip_js_string(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            byte if byte == quote => return index + 1,
            b'\n' => return index,
            _ => index += 1,
        }
    }
    bytes.len()
}

fn skip_js_template(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'`' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// Reads a template literal that contains no `${}` substitutions. Dynamic
/// imports with interpolated paths cannot be discovered statically.
fn read_js_template_literal(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    if start >= bytes.len() || bytes[start] != b'`' {
        return None;
    }
    let mut value = String::new();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'`' => return Some((value, index + 1)),
            b'\\' => {
                let escape = *bytes.get(index + 1)?;
                match escape {
                    b'n' => value.push('\n'),
                    b't' => value.push('\t'),
                    b'r' => value.push('\r'),
                    other => value.push(other as char),
                }
                index += 2;
            }
            b'$' if bytes.get(index + 1) == Some(&b'{') => return None,
            byte => {
                value.push(byte as char);
                index += 1;
            }
        }
    }
    None
}

fn read_js_string(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    if start >= bytes.len() || (bytes[start] != b'\'' && bytes[start] != b'"') {
        return None;
    }
    let quote = bytes[start];
    let mut value = String::new();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            byte if byte == quote => return Some((value, index + 1)),
            b'\\' => {
                let escape = *bytes.get(index + 1)?;
                match escape {
                    b'n' => value.push('\n'),
                    b't' => value.push('\t'),
                    b'r' => value.push('\r'),
                    b'b' => value.push('\u{8}'),
                    b'f' => value.push('\u{c}'),
                    b'v' => value.push('\u{b}'),
                    b'0' => value.push('\0'),
                    b'u' => {
                        let hex = bytes.get(index + 2..index + 6)?;
                        let text = std::str::from_utf8(hex).ok()?;
                        let code = u32::from_str_radix(text, 16).ok()?;
                        value.push(char::from_u32(code)?);
                        index += 4;
                    }
                    other => value.push(other as char),
                }
                index += 2;
            }
            b'\n' => return None,
            byte => {
                value.push(byte as char);
                index += 1;
            }
        }
    }
    None
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' || byte >= 0x80
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

struct CssRule {
    selector: Selector,
    specificity: usize,
    order: usize,
    declarations: String,
}

fn collect_node_styles(
    document: &Html,
    external_stylesheets: &[String],
    viewport_width: f32,
) -> HashMap<NodeId, String> {
    let mut rules = Vec::new();
    let mut order = 0;
    for stylesheet in external_stylesheets {
        rules.extend(parse_css_rules(stylesheet, &mut order, viewport_width));
    }

    if let Ok(style_selector) = Selector::parse("style") {
        for style in document.select(&style_selector) {
            let stylesheet = style.text().collect::<String>();
            rules.extend(parse_css_rules(&stylesheet, &mut order, viewport_width));
        }
    }

    let mut matched_rules: HashMap<NodeId, Vec<(usize, usize, String)>> = HashMap::new();
    for rule in rules {
        for element in document.select(&rule.selector) {
            matched_rules.entry(element.id()).or_default().push((
                rule.specificity,
                rule.order,
                rule.declarations.clone(),
            ));
        }
    }

    matched_rules
        .into_iter()
        .map(|(node_id, mut declarations)| {
            declarations.sort_by_key(|(specificity, order, _)| (*specificity, *order));
            let declarations = declarations
                .into_iter()
                .map(|(_, _, declarations)| declarations)
                .collect::<Vec<_>>()
                .join(";");
            (node_id, declarations)
        })
        .collect()
}

fn parse_css_rules(stylesheet: &str, order: &mut usize, viewport_width: f32) -> Vec<CssRule> {
    let stylesheet = strip_css_comments(stylesheet);
    parse_css_rules_inner(&stylesheet, order, viewport_width, 0)
}

/// Reglas de primer nivel de un bloque CSS. Los `@media` que coinciden con el
/// ancho del viewport se expanden reutilizando el mismo contador de orden, de
/// modo que la cascada los trata como si estuvieran escritos en su lugar; el
/// resto de at-rules (`@supports`, `@font-face`, `@keyframes`…) se omiten.
fn parse_css_rules_inner(
    stylesheet: &str,
    order: &mut usize,
    viewport_width: f32,
    nesting: usize,
) -> Vec<CssRule> {
    let mut rules = Vec::new();
    if nesting > 8 {
        return rules;
    }
    let mut selector_start = 0;
    let mut current_rule = None;
    let mut depth = 0usize;

    for (index, character) in stylesheet.char_indices() {
        match character {
            '{' => {
                if depth == 0 {
                    current_rule = Some((selector_start, index + character.len_utf8()));
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some((start, declarations_start)) = current_rule.take() {
                        let selector_end = declarations_start - 1;
                        let selector_list = stylesheet[start..selector_end].trim();
                        let declarations = stylesheet[declarations_start..index].trim();
                        if let Some(condition) = supports_condition(selector_list) {
                            if supports_query_matches(condition) {
                                rules.extend(parse_css_rules_inner(
                                    declarations,
                                    order,
                                    viewport_width,
                                    nesting + 1,
                                ));
                            }
                        } else if let Some(condition) = media_condition(selector_list) {
                            if media_query_matches(condition, viewport_width) {
                                rules.extend(parse_css_rules_inner(
                                    declarations,
                                    order,
                                    viewport_width,
                                    nesting + 1,
                                ));
                            }
                        } else if !selector_list.starts_with('@') && !declarations.is_empty() {
                            for selector_text in split_selector_list(selector_list) {
                                let selector_text = selector_text.trim();
                                if selector_text.is_empty() {
                                    continue;
                                }
                                // `scraper` conoce `html`, no el pseudo-elemento
                                // `:root`, que en HTML es siempre el elemento raíz.
                                let selector_text = if selector_text.eq_ignore_ascii_case(":root") {
                                    "html"
                                } else {
                                    selector_text
                                };
                                if let Ok(selector) = Selector::parse(selector_text) {
                                    rules.push(CssRule {
                                        selector,
                                        specificity: css_specificity(selector_text),
                                        order: *order,
                                        declarations: declarations.to_owned(),
                                    });
                                    *order += 1;
                                }
                            }
                        }
                    }
                    selector_start = index + character.len_utf8();
                }
            }
            _ => {}
        }
    }

    rules
}

/// Divide una lista de selectores por comas de primer nivel (sin cortar
/// dentro de `:is(...)`, `[...]`, etc.).
fn split_selector_list(selectors: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (index, character) in selectors.char_indices() {
        match character {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&selectors[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&selectors[start..]);
    parts
}

/// Preludio de un `@supports`, o `None` si el bloque no es un `@supports`.
fn supports_condition(prelude: &str) -> Option<&str> {
    let prelude = prelude.trim();
    let keyword = prelude.get(..9)?;
    keyword
        .eq_ignore_ascii_case("@supports")
        .then(|| prelude[9..].trim())
}

/// Propiedades que Wisp interpreta de verdad. `@supports` se evalúa contra
/// esta lista: si el motor no implementa la declaración, el bloque se omite,
/// que es exactamente lo que hace un navegador con `@supports`.
const SUPPORTED_CSS_PROPERTIES: &[&str] = &[
    "align-items",
    "background",
    "background-color",
    "background-image",
    "border",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-style",
    "border-bottom-width",
    "border-color",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-radius",
    "border-right",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-style",
    "border-top",
    "border-top-color",
    "border-top-style",
    "border-top-width",
    "border-width",
    "box-sizing",
    "column-gap",
    "color",
    "display",
    "flex",
    "flex-basis",
    "flex-direction",
    "flex-grow",
    "flex-shrink",
    "flex-wrap",
    "font-size",
    "font-weight",
    "gap",
    "grid-column",
    "grid-column-end",
    "grid-column-start",
    "grid-row",
    "grid-row-end",
    "grid-row-start",
    "grid-template-columns",
    "grid-template-rows",
    "height",
    "justify-content",
    "line-height",
    "list-style",
    "list-style-type",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-height",
    "max-width",
    "min-height",
    "min-width",
    "overflow-wrap",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "row-gap",
    "text-align",
    "text-decoration",
    "text-transform",
    "top",
    "right",
    "bottom",
    "left",
    "visibility",
    "white-space",
    "width",
    "word-break",
];

/// Evalúa una condición de `@supports`: `not`, `and`, `or`,
/// `(propiedad: valor)` y `selector(...)`.
fn supports_query_matches(condition: &str) -> bool {
    let condition = condition.trim();
    if condition.is_empty() {
        return false;
    }
    if condition
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("not "))
    {
        return !supports_query_matches(&condition[4..]);
    }
    if let Some((left, right)) = split_top_level_keyword(condition, " or ") {
        return supports_query_matches(left) || supports_query_matches(right);
    }
    if let Some((left, right)) = split_top_level_keyword(condition, " and ") {
        return supports_query_matches(left) && supports_query_matches(right);
    }
    if let Some(inner) = strip_parentheses(condition) {
        return supports_declaration_supported(inner);
    }
    if let Some(inner) = strip_css_function(condition, "selector") {
        // `scraper` implementa el motor de selectores que usa Wisp.
        return !inner.trim().is_empty();
    }
    false
}

fn supports_declaration_supported(declaration: &str) -> bool {
    let Some((name, value)) = declaration.split_once(':') else {
        return false;
    };
    let name = name.trim().to_ascii_lowercase();
    let value = value.trim().to_ascii_lowercase();
    if name.starts_with("--") {
        return true;
    }
    match name.as_str() {
        // `@supports (display: grid)` es el caso más habitual: sólo cuenta la
        // variante que el motor sabe maquetar.
        "display" => matches!(
            value.as_str(),
            "block"
                | "inline"
                | "inline-block"
                | "flex"
                | "inline-flex"
                | "grid"
                | "inline-grid"
                | "none"
        ),
        "position" => matches!(value.as_str(), "static" | "relative" | "absolute"),
        "visibility" => matches!(value.as_str(), "visible" | "hidden"),
        _ => SUPPORTED_CSS_PROPERTIES.contains(&name.as_str()),
    }
}

/// Divide por la primera aparición de `keyword` en el nivel superior de
/// paréntesis y corchetes.
fn split_top_level_keyword<'a>(value: &'a str, keyword: &str) -> Option<(&'a str, &'a str)> {
    let bytes = value.as_bytes();
    let keyword_bytes = keyword.as_bytes();
    let mut depth = 0i32;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'(' | b'[' => {
                depth += 1;
                index += 1;
            }
            b')' | b']' => {
                depth = (depth - 1).max(0);
                index += 1;
            }
            _ => {
                if depth == 0
                    && index + keyword_bytes.len() <= bytes.len()
                    && bytes[index..index + keyword_bytes.len()].eq_ignore_ascii_case(keyword_bytes)
                {
                    return Some((
                        value[..index].trim(),
                        value[index + keyword_bytes.len()..].trim(),
                    ));
                }
                index += 1;
            }
        }
    }
    None
}

/// Condición de un `@media`, o `None` si el preludio no es una media query.
fn media_condition(prelude: &str) -> Option<&str> {
    let prelude = prelude.trim();
    let keyword = prelude.get(..6)?;
    keyword
        .eq_ignore_ascii_case("@media")
        .then(|| prelude[6..].trim())
}

/// Evalúa una media query contra el ancho del viewport. Soporta listas
/// separadas por comas, `not`/`only`, tipos `screen`/`all`/`print` y las
/// features de ancho, orientación y preferencias más habituales.
fn media_query_matches(condition: &str, viewport_width: f32) -> bool {
    let condition = condition.trim();
    if condition.is_empty() {
        return true;
    }
    split_selector_list(condition)
        .iter()
        .any(|query| media_query_part_matches(query, viewport_width))
}

fn media_query_part_matches(query: &str, viewport_width: f32) -> bool {
    let query = query.trim().to_ascii_lowercase();
    let (negated, query) = match query.split_once(char::is_whitespace) {
        Some((keyword, rest)) if keyword == "not" => (true, rest.trim().to_owned()),
        Some((keyword, rest)) if keyword == "only" => (false, rest.trim().to_owned()),
        _ => (false, query),
    };
    let mut matched = true;
    for part in split_media_conditions(&query) {
        if part.is_empty() {
            continue;
        }
        let value = match part
            .strip_prefix('(')
            .and_then(|inner| inner.strip_suffix(')'))
        {
            Some(feature) => media_feature_matches(feature, viewport_width),
            None => matches!(part, "all" | "screen"),
        };
        if !value {
            matched = false;
            break;
        }
    }
    matched != negated
}

/// Separa las condiciones unidas por `and` fuera de paréntesis.
fn split_media_conditions(query: &str) -> Vec<&str> {
    let bytes = query.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'a' if depth == 0
                && bytes.get(index..index + 3) == Some(b"and")
                && index > 0
                && bytes[index - 1].is_ascii_whitespace()
                && bytes.get(index + 3).is_some_and(u8::is_ascii_whitespace) =>
            {
                parts.push(query[start..index].trim());
                start = index + 3;
                index += 3;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(query[start..].trim());
    parts
}

/// Sintaxis de rangos de Media Queries 4: `width >= 600px`, `600px < width`.
fn media_range_matches(feature: &str, viewport_width: f32) -> Option<bool> {
    for operator in [">=", "<=", ">", "<"] {
        let Some((left, right)) = feature.split_once(operator) else {
            continue;
        };
        let (left, right) = (left.trim(), right.trim());
        let (width_on_left, value) = if matches!(left, "width" | "device-width") {
            (true, right)
        } else if matches!(right, "width" | "device-width") {
            (false, left)
        } else {
            continue;
        };
        let Some(value) = media_length(value) else {
            continue;
        };
        // Con el ancho a la derecha el operador se lee al revés.
        let operator = match (operator, width_on_left) {
            (">=", false) => "<=",
            ("<=", false) => ">=",
            (">", false) => "<",
            ("<", false) => ">",
            (_, true) => operator,
            _ => continue,
        };
        return Some(match operator {
            ">=" => viewport_width >= value,
            "<=" => viewport_width <= value,
            ">" => viewport_width > value,
            _ => viewport_width < value,
        });
    }
    None
}

fn media_feature_matches(feature: &str, viewport_width: f32) -> bool {
    let feature = feature.trim();
    if let Some(result) = media_range_matches(feature, viewport_width) {
        return result;
    }
    let Some((name, value)) = feature.split_once(':') else {
        // Condiciones booleanas: Wisp asume un escritorio con puntero y color.
        return matches!(
            feature,
            "hover" | "any-hover" | "pointer" | "any-pointer" | "color"
        );
    };
    let name = name.trim();
    let value = value.trim();
    match name {
        "min-width" | "min-device-width" => {
            media_length(value).is_some_and(|min| viewport_width >= min)
        }
        "max-width" | "max-device-width" => {
            media_length(value).is_some_and(|max| viewport_width <= max)
        }
        "width" | "device-width" => {
            media_length(value).is_some_and(|width| (viewport_width - width).abs() <= 1.0)
        }
        // La altura del viewport no está disponible antes de maquetar: la
        // orientación se aproxima por el ancho, como en una ventana de escritorio.
        "orientation" => match value {
            "landscape" => viewport_width >= 640.0,
            "portrait" => viewport_width < 640.0,
            _ => false,
        },
        // Wisp pinta siempre sobre blanco y no reproduce animaciones.
        "prefers-color-scheme" => value == "light",
        "prefers-reduced-motion" => value == "reduce",
        "hover" | "any-hover" | "pointer" | "any-pointer" => {
            matches!(value, "hover" | "fine")
        }
        _ => false,
    }
}

fn media_length(value: &str) -> Option<f32> {
    parse_absolute_length(value, ROOT_FONT_SIZE)
}

fn strip_css_comments(stylesheet: &str) -> String {
    let mut result = String::with_capacity(stylesheet.len());
    let mut remaining = stylesheet;
    while let Some(start) = remaining.find("/*") {
        result.push_str(&remaining[..start]);
        let after_comment_start = &remaining[start + 2..];
        let Some(end) = after_comment_start.find("*/") else {
            return result;
        };
        remaining = &after_comment_start[end + 2..];
    }
    result.push_str(remaining);
    result
}

/// Especificidad (a, b, c) de CSS: IDs, clases/atributos/pseudo-clases y
/// elementos/pseudo-elementos, empaquetada en un entero comparable.
fn css_specificity(selector: &str) -> usize {
    let bytes = selector.as_bytes();
    let mut ids = 0usize;
    let mut classes = 0usize;
    let mut elements = 0usize;
    let mut index = 0usize;
    let mut compound_start = true;
    while index < bytes.len() {
        match bytes[index] {
            b'#' => {
                ids += 1;
                compound_start = false;
                index = skip_css_identifier(bytes, index + 1);
            }
            b'.' => {
                classes += 1;
                compound_start = false;
                index = skip_css_identifier(bytes, index + 1);
            }
            b'[' => {
                classes += 1;
                compound_start = false;
                index = skip_css_bracket(bytes, index);
            }
            b':' => {
                if bytes.get(index + 1) == Some(&b':') {
                    elements += 1;
                    index = skip_css_identifier(bytes, index + 2);
                } else {
                    classes += 1;
                    index = skip_css_identifier(bytes, index + 1);
                }
                compound_start = false;
            }
            b'*' => {
                compound_start = false;
                index += 1;
            }
            b',' | b'>' | b'+' | b'~' => {
                compound_start = true;
                index += 1;
            }
            byte if byte.is_ascii_whitespace() => {
                compound_start = true;
                index += 1;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'-' => {
                index = skip_css_identifier(bytes, index);
                if compound_start {
                    elements += 1;
                }
                compound_start = false;
            }
            _ => index += 1,
        }
    }
    ids * 1_000_000 + classes * 1_000 + elements
}

fn skip_css_identifier(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'_' | b'-' | b'\\' | b'|')
            || byte >= 0x80
        {
            index += 1;
        } else {
            break;
        }
    }
    index
}

fn skip_css_bracket(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    while index < bytes.len() && bytes[index] != b']' {
        index += 1;
    }
    (index + 1).min(bytes.len())
}

fn title_from_html(html: &str) -> Result<String> {
    let document = Html::parse_document(html);
    let title_selector = Selector::parse("title")
        .map_err(|error| anyhow::anyhow!("could not create title selector: {error}"))?;
    Ok(document
        .select(&title_selector)
        .next()
        .map(|element| element.text().collect::<String>().trim().to_owned())
        .unwrap_or_default())
}

fn convert_node(
    node: NodeRef<'_, Node>,
    depth: usize,
    node_styles: &HashMap<NodeId, String>,
    page_url: Option<&str>,
) -> Result<Option<DomNode>> {
    ensure!(
        depth <= MAX_TREE_DEPTH,
        "HTML nesting exceeds Wisp's depth limit"
    );
    match node.value() {
        Node::Text(text) => {
            let text = text.text.to_string();
            if text.trim().is_empty() {
                return Ok(None);
            }
            Ok(Some(DomNode {
                tag: None,
                text,
                inline_style: String::new(),
                href: None,
                image: None,
                background_image: None,
                children: Vec::new(),
                style_cache: OnceLock::new(),
            }))
        }
        Node::Element(_) => {
            let element =
                ElementRef::wrap(node).ok_or_else(|| anyhow::anyhow!("invalid HTML element"))?;
            let tag = element.value().name().to_ascii_lowercase();
            if matches!(
                tag.as_str(),
                "head" | "script" | "style" | "title" | "meta" | "link" | "noscript"
            ) {
                return Ok(None);
            }

            let inline_style = format!(
                "{};{}",
                node_styles
                    .get(&node.id())
                    .map(String::as_str)
                    .unwrap_or_default(),
                element.attr("style").unwrap_or_default()
            );
            let image = if tag == "img" {
                Some(DomImage {
                    src: element
                        .attr("src")
                        .map(str::trim)
                        .filter(|src| !src.is_empty())
                        .or_else(|| element.attr("srcset").and_then(largest_srcset_candidate))
                        .and_then(|src| resolve_image_key(page_url, src))
                        .unwrap_or_default(),
                    alt: element.attr("alt").unwrap_or_default().to_owned(),
                    width: element.attr("width").and_then(parse_length),
                    height: element.attr("height").and_then(parse_length),
                })
            } else {
                None
            };
            let background_image = node_background_image(&inline_style, page_url);
            let mut children = Vec::new();
            for child in element.children() {
                if let Some(child) = convert_node(child, depth + 1, node_styles, page_url)? {
                    children.push(child);
                }
            }
            Ok(Some(DomNode {
                tag: Some(tag),
                text: String::new(),
                inline_style,
                href: element.attr("href").map(str::to_owned),
                image,
                background_image,
                children,
                style_cache: OnceLock::new(),
            }))
        }
        Node::Document | Node::Fragment => {
            let mut children = Vec::new();
            for child in node.children() {
                if let Some(child) = convert_node(child, depth + 1, node_styles, page_url)? {
                    children.push(child);
                }
            }
            Ok(Some(DomNode {
                tag: None,
                text: String::new(),
                inline_style: String::new(),
                href: None,
                image: None,
                background_image: None,
                children,
                style_cache: OnceLock::new(),
            }))
        }
        Node::Doctype(_) | Node::Comment(_) | Node::ProcessingInstruction(_) => Ok(None),
    }
}

type CustomProperties = HashMap<String, String>;

/// Resuelve las custom properties de CSS (`var(--token, respaldo)`) sobre el
/// árbol ya convertido. Se heredan como en un navegador: un `--color` definido
/// en `:root` o en cualquier ancestro llega a todos sus descendientes.
fn resolve_custom_properties(roots: &mut [DomNode]) {
    let inherited = CustomProperties::new();
    for root in roots {
        resolve_custom_properties_node(root, &inherited);
    }
}

/// Los documentos preparados se cachean por baldes de 8 px: un redimensionado
/// reevalúa el CSS al cruzar un balde (~4 px de error en las media queries)
/// en lugar de en cada píxel.
const PREPARED_WIDTH_BUCKET: f32 = 8.0;

fn prepared_width_bucket(viewport_width: f32) -> f32 {
    let width = viewport_width.clamp(1.0, 16_384.0);
    (width / PREPARED_WIDTH_BUCKET).round() * PREPARED_WIDTH_BUCKET
}

fn resolve_custom_properties_node(node: &mut DomNode, inherited: &CustomProperties) {
    if node.inline_style.is_empty() {
        for child in &mut node.children {
            resolve_custom_properties_node(child, inherited);
        }
        return;
    }

    let mut definitions = Vec::new();
    for declaration in node.inline_style.split(';') {
        let Some((name, value)) = declaration.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.starts_with("--") {
            definitions.push((name.to_owned(), value.trim().to_owned()));
        }
    }
    let has_vars = node.inline_style.contains("var(");
    // La mayoría de los nodos no declaran variables ni usan `var()`: se
    // reutiliza el mapa heredado sin clonarlo.
    if definitions.is_empty() && !has_vars {
        for child in &mut node.children {
            resolve_custom_properties_node(child, inherited);
        }
        return;
    }

    let mut local = inherited.clone();
    for (name, raw) in &definitions {
        local.insert(name.clone(), raw.clone());
    }
    // Los valores pueden referirse a otras custom properties, incluso a las
    // definidas más abajo en el mismo bloque.
    for (name, raw) in &definitions {
        if let Some(resolved) = substitute_vars(raw, &local) {
            local.insert(name.clone(), resolved);
        }
    }

    if has_vars {
        let mut rewritten = Vec::new();
        for declaration in node.inline_style.split(';') {
            let Some((name, value)) = declaration.split_once(':') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if name.starts_with("--") {
                if let Some(value) = local.get(name) {
                    rewritten.push(format!("{name}:{value}"));
                }
                continue;
            }
            // Una `var()` sin valor hace la declaración inválida en tiempo de
            // cálculo: se descarta y gana la siguiente de la cascada.
            if let Some(value) = substitute_vars(value.trim(), &local) {
                rewritten.push(format!("{name}:{value}"));
            }
        }
        node.inline_style = rewritten.join(";");
    }

    for child in &mut node.children {
        resolve_custom_properties_node(child, &mut local);
    }
}

fn substitute_vars(value: &str, properties: &CustomProperties) -> Option<String> {
    substitute_vars_depth(value, properties, 0)
}

fn substitute_vars_depth(
    value: &str,
    properties: &CustomProperties,
    depth: usize,
) -> Option<String> {
    if depth > 16 {
        return None;
    }
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find("var(") {
        output.push_str(&rest[..index]);
        let after = &rest[index + 4..];
        let end = matching_paren(after)?;
        let inner = &after[..end];
        let (name, fallback) = match inner.split_once(',') {
            Some((name, fallback)) => (name.trim(), Some(fallback.trim())),
            None => (inner.trim(), None),
        };
        let resolved = match properties.get(name) {
            Some(value) => value.clone(),
            None => substitute_vars_depth(fallback?, properties, depth + 1)?,
        };
        if resolved.contains("var(") {
            output.push_str(&substitute_vars_depth(&resolved, properties, depth + 1)?);
        } else {
            output.push_str(&resolved);
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Some(output)
}

/// Índice del `)` que cierra el paréntesis ya abierto antes de `text`.
fn matching_paren(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (index, character) in text.char_indices() {
        match character {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(index),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Ancho de caja (border-box) que ocupa un bloque: `width` o el ancho
/// disponible, acotado por `min-width`/`max-width`. Con `content-box` el
/// padding y el borde se suman al valor declarado, como en CSS.
fn outer_width_for(style: Style, available_width: f32) -> f32 {
    let padding = style.padding.map(|side| side.resolve(available_width));
    let border = style.border_width.map(|side| side.resolve(available_width));
    let margin = style.margin.map(|side| side.resolve(available_width));
    let chrome = padding[1] + padding[3] + border[1] + border[3];
    let to_outer = |length: Length| {
        let value = length.resolve(available_width);
        if style.box_sizing == BoxSizingKind::ContentBox {
            value + chrome
        } else {
            value
        }
    };
    let mut width = match style.width {
        Some(width) => to_outer(width),
        None => available_width - margin[1] - margin[3],
    };
    if let Some(max) = style.max_width {
        width = width.min(to_outer(max));
    }
    if let Some(min) = style.min_width {
        width = width.max(to_outer(min));
    }
    width.max(0.0).min(available_width.max(0.0))
}

/// Desplazamiento de pintado de `position: relative`: `left` manda sobre
/// `right` y `top` sobre `bottom`; el flujo no cambia.
fn relative_offset(style: Style, basis: f32) -> (f32, f32) {
    if style.position != PositionKind::Relative {
        return (0.0, 0.0);
    }
    let left = style.insets[3].map(|length| length.resolve(basis));
    let right = style.insets[1].map(|length| length.resolve(basis));
    let top = style.insets[0].map(|length| length.resolve(basis));
    let bottom = style.insets[2].map(|length| length.resolve(basis));
    (
        left.unwrap_or_else(|| -right.unwrap_or(0.0)),
        top.unwrap_or_else(|| -bottom.unwrap_or(0.0)),
    )
}

/// Pinta un hijo `position: absolute`: se coloca respecto al caja de
/// contenido del ancestro que lo contiene y no altera el flujo del padre.
/// Con `left` y `right` juntos se estira a lo ancho; con solo `bottom` se mide
/// primero su altura para apoyarlo en el borde inferior.
#[allow(clippy::too_many_arguments)]
fn layout_absolute_child(
    child: &DomNode,
    parent_style: Style,
    content_x: f32,
    content_y: f32,
    content_width: f32,
    content_height: f32,
    images: &HashMap<String, ImageResource>,
    commands: &mut Vec<PaintCommand>,
    depth: usize,
    measure_only: bool,
    measure_cache: &mut MeasureCache,
) -> Result<()> {
    let child_style = cached_style(child, parent_style);
    let inset =
        |index: usize| child_style.insets[index].map(|length| length.resolve(content_width));
    let (left, right) = (inset(3), inset(1));
    let (top, bottom) = (inset(0), inset(2));
    let width = match (left, right) {
        (Some(left), Some(right)) => (content_width - left - right).max(1.0),
        _ => content_width,
    };
    let x = match (left, right) {
        (Some(left), _) => content_x + left,
        // Sin `left`, la caja se apoya en el borde derecho con su ancho propio.
        (None, Some(right)) if child_style.width.is_some() => {
            (content_x + content_width - outer_width_for(child_style, content_width) - right)
                .max(content_x)
        }
        _ => content_x,
    };
    // Se mide siempre: hace falta la altura para `bottom` y así se pinta una
    // sola vez con la posición ya resuelta.
    let measured = layout_block(
        child,
        x,
        0.0,
        width,
        parent_style,
        images,
        &mut Vec::new(),
        depth + 1,
        true,
        measure_cache,
    )?;
    let y = match (top, bottom) {
        (Some(top), _) => content_y + top,
        (None, Some(bottom)) => content_y + (content_height - measured - bottom).max(0.0),
        _ => content_y,
    };
    layout_block(
        child,
        x,
        y,
        width,
        parent_style,
        images,
        commands,
        depth + 1,
        measure_only,
        measure_cache,
    )?;
    Ok(())
}

fn layout_block(
    node: &DomNode,
    x: f32,
    y: f32,
    available_width: f32,
    inherited: Style,
    images: &HashMap<String, ImageResource>,
    commands: &mut Vec<PaintCommand>,
    depth: usize,
    measure_only: bool,
    measure_cache: &mut MeasureCache,
) -> Result<f32> {
    ensure!(
        depth <= MAX_TREE_DEPTH,
        "layout nesting exceeds Wisp's depth limit"
    );

    let style = cached_style(node, inherited);
    if style.display == Display::None {
        return Ok(0.0);
    }

    let margin = style.margin.map(|side| side.resolve(available_width));
    let padding = style.padding.map(|side| side.resolve(available_width));
    let border = style.border_width.map(|side| side.resolve(available_width));
    let outer_width = outer_width_for(style, available_width);
    // `margin-left/right: auto` reparte el espacio libre que queda en la
    // línea; con ambos en `auto` la caja queda centrada.
    let remaining = (available_width - outer_width).max(0.0);
    let left_offset = match (style.margin_auto[3], style.margin_auto[1]) {
        (true, true) => remaining / 2.0,
        (true, false) => (remaining - margin[1]).max(0.0),
        (false, _) => margin[3],
    };
    let outer_x = x + left_offset;
    let content_width = (outer_width - border[1] - border[3] - padding[1] - padding[3]).max(1.0);
    let content_x = outer_x + border[3] + padding[3];
    let box_y = y + margin[0];
    let content_y = box_y + border[0] + padding[0];
    let command_start = commands.len();
    let mut cursor_y = content_y;
    let mut inline_items = Vec::new();

    if matches!(style.display, Display::Flex | Display::Grid) {
        cursor_y += layout_taffy_children(
            node,
            content_x,
            content_y,
            content_width,
            style,
            images,
            commands,
            depth + 1,
            measure_only,
            measure_cache,
        )?;
    } else {
        let list_kind = matches!(node.tag.as_deref(), Some("ul" | "ol"))
            .then_some(style.list_style)
            .filter(|kind| *kind != ListStyleKind::None);
        let mut list_index = 0u32;
        let mut absolute_children: Vec<&DomNode> = Vec::new();
        for child in &node.children {
            if is_hidden(child) {
                continue;
            }
            let child_style = cached_style(child, style);
            // Los absolutos salen del flujo y se pintan al final del bloque.
            if child_style.position == PositionKind::Absolute {
                absolute_children.push(child);
                continue;
            }
            if is_block(child) {
                let (run_height, _) = flush_inline_runs(
                    &mut inline_items,
                    content_x,
                    cursor_y,
                    content_width,
                    images,
                    commands,
                    style.text_align,
                    measure_only,
                );
                cursor_y += run_height;
                if let Some(kind) = list_kind
                    && child.tag.as_deref() == Some("li")
                    && !measure_only
                {
                    list_index += 1;
                    paint_list_marker(
                        child, kind, list_index, content_x, cursor_y, style, commands,
                    );
                }
                // `position: relative` desplaza el pintado, no el flujo.
                let (offset_x, offset_y) = relative_offset(child_style, content_width);
                cursor_y += layout_block(
                    child,
                    content_x + offset_x,
                    cursor_y + offset_y,
                    content_width,
                    style,
                    images,
                    commands,
                    depth + 1,
                    measure_only,
                    measure_cache,
                )?;
            } else {
                collect_inline_runs(child, style, None, images, &mut inline_items);
            }
        }
        let (run_height, _) = flush_inline_runs(
            &mut inline_items,
            content_x,
            cursor_y,
            content_width,
            images,
            commands,
            style.text_align,
            measure_only,
        );
        cursor_y += run_height;

        let flow_height = (cursor_y - content_y).max(0.0);
        for child in absolute_children {
            layout_absolute_child(
                child,
                style,
                content_x,
                content_y,
                content_width,
                flow_height,
                images,
                commands,
                depth,
                measure_only,
                measure_cache,
            )?;
        }
    }

    let content_height = (cursor_y - content_y).max(0.0);
    let vertical_chrome = padding[0] + padding[2] + border[0] + border[2];
    let to_content_height = |length: Length| {
        let value = length.resolve(available_width);
        if style.box_sizing == BoxSizingKind::BorderBox {
            (value - vertical_chrome).max(0.0)
        } else {
            value
        }
    };
    // Un alto en porcentaje sigue resolviendo como `auto` (comportamiento
    // correcto cuando el contenedor no tiene altura definida).
    let explicit_height = style
        .height
        .filter(|length| length.is_absolute())
        .map(|length| length.resolve(0.0));
    let mut box_height = match explicit_height {
        Some(height) => {
            if style.box_sizing == BoxSizingKind::BorderBox {
                (height - vertical_chrome).max(0.0)
            } else {
                height
            }
        }
        // `aspect-ratio` con un solo eje definido: el alto lo determina el
        // ancho del bloque (o su caja de borde con `box-sizing: border-box`).
        None => match style.aspect_ratio {
            Some(ratio) if ratio > 0.0 => {
                let horizontal = padding[1] + padding[3] + border[1] + border[3];
                let vertical = padding[0] + padding[2] + border[0] + border[2];
                match style.box_sizing {
                    BoxSizingKind::BorderBox => {
                        ((content_width + horizontal) / ratio - vertical).max(0.0)
                    }
                    BoxSizingKind::ContentBox => content_width / ratio,
                }
            }
            _ => content_height,
        },
    };
    if let Some(max) = style.max_height {
        box_height = box_height.min(to_content_height(max));
    }
    if let Some(min) = style.min_height {
        box_height = box_height.max(to_content_height(min));
    }
    let border_box_height = border[0] + padding[0] + box_height + padding[2] + border[2];
    if !measure_only && style.visibility == VisibilityKind::Visible {
        let radius = resolve_radius(style.border_radius, outer_width, border_box_height);
        if let Some(color) = style.background {
            commands.insert(
                command_start,
                PaintCommand::FillRect {
                    rect: Rect {
                        x: outer_x,
                        y: box_y,
                        width: outer_width,
                        height: border_box_height,
                    },
                    color,
                    radius,
                },
            );
        }
        if let Some(image) = &node.background_image {
            commands.insert(
                command_start + usize::from(style.background.is_some()),
                PaintCommand::BackgroundImage {
                    rect: Rect {
                        x: outer_x,
                        y: box_y,
                        width: outer_width,
                        height: border_box_height,
                    },
                    radius,
                    image: image.clone(),
                },
            );
        }
        paint_border(
            outer_x,
            box_y,
            outer_width,
            border_box_height,
            border,
            &style,
            radius,
            commands,
        );
    }
    // `overflow: hidden|auto|scroll`: el contenido (incluidos los hijos
    // absolutos) queda recortado al padding box, como en un navegador. El
    // recorte va antes del fondo para que éste también se limite.
    if style.overflow_clip && !measure_only {
        let clip = Rect {
            x: outer_x + border[3],
            y: box_y + border[0],
            width: (outer_width - border[1] - border[3]).max(0.0),
            height: (border_box_height - border[0] - border[2]).max(0.0),
        };
        commands.insert(command_start, PaintCommand::Clip { rect: clip });
        commands.push(PaintCommand::ClipEnd);
    }

    Ok(margin[0] + border_box_height + margin[2])
}

/// Texto del marcador de una lista para `list-style-type`.
fn list_marker_text(kind: ListStyleKind, index: u32) -> String {
    match kind {
        ListStyleKind::Disc => "•".to_owned(),
        ListStyleKind::Circle => "◦".to_owned(),
        ListStyleKind::Square => "▪".to_owned(),
        ListStyleKind::None => String::new(),
        ListStyleKind::Decimal => format!("{index}."),
        ListStyleKind::LowerAlpha => format!("{}.", alpha_label(index, false)),
        ListStyleKind::UpperAlpha => format!("{}.", alpha_label(index, true)),
        ListStyleKind::LowerRoman => format!("{}.", roman_numeral(index)).to_lowercase(),
        ListStyleKind::UpperRoman => format!("{}.", roman_numeral(index)),
    }
}

/// Etiqueta alfabética de una lista ordenada (1 → a, 27 → aa).
fn alpha_label(mut index: u32, uppercase: bool) -> String {
    index = index.max(1);
    let mut label = String::new();
    while index > 0 {
        let letter = (b'a' + ((index - 1) % 26) as u8) as char;
        label.insert(
            0,
            if uppercase {
                letter.to_ascii_uppercase()
            } else {
                letter
            },
        );
        index = (index - 1) / 26;
    }
    label
}

fn roman_numeral(mut index: u32) -> String {
    if index == 0 || index > 3999 {
        return index.to_string();
    }
    const TABLE: [(u32, &str); 13] = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut numeral = String::new();
    for (value, symbol) in TABLE {
        while index >= value {
            numeral.push_str(symbol);
            index -= value;
        }
    }
    numeral
}

fn parse_list_style(value: &str) -> Option<ListStyleKind> {
    Some(match value.trim().to_ascii_lowercase().as_str() {
        "disc" => ListStyleKind::Disc,
        "circle" => ListStyleKind::Circle,
        "square" => ListStyleKind::Square,
        "none" => ListStyleKind::None,
        "decimal" => ListStyleKind::Decimal,
        "lower-alpha" | "lower-latin" => ListStyleKind::LowerAlpha,
        "upper-alpha" | "upper-latin" => ListStyleKind::UpperAlpha,
        "lower-roman" => ListStyleKind::LowerRoman,
        "upper-roman" => ListStyleKind::UpperRoman,
        _ => return None,
    })
}

/// Viñeta/número de un `<li>`: se pinta en el margen izquierdo que crea el
/// `padding-left` del `ul`/`ol`, alineado con el borde derecho del marcador.
fn paint_list_marker(
    child: &DomNode,
    kind: ListStyleKind,
    index: u32,
    content_x: f32,
    y: f32,
    parent_style: Style,
    commands: &mut Vec<PaintCommand>,
) {
    let style = cached_style(child, parent_style);
    if style.visibility == VisibilityKind::Hidden {
        return;
    }
    let marker = list_marker_text(kind, index);
    if marker.is_empty() {
        return;
    }
    let marker_width =
        text_width_weighted(&marker, style.font_size, is_bold_weight(style.font_weight));
    commands.push(PaintCommand::Text {
        x: content_x - 10.0 - marker_width,
        y,
        content: marker,
        color: style.color,
        font_size: style.font_size,
        font_weight: style.font_weight,
        href: None,
    });
}

/// `border-radius`: px directo; `%` sobre la mitad del lado menor (aproxima
/// la elipse CSS con una esquina circular, suficiente para `50%` → círculo).
fn resolve_radius(radius: Length, width: f32, height: f32) -> f32 {
    let basis = width.min(height);
    let radius = match radius {
        Length::Px(px) => px,
        Length::Percent(percent) => basis * percent / 100.0,
        Length::Sum { .. } | Length::Function { .. } => radius.resolve(basis),
    };
    radius.max(0.0).min(basis / 2.0)
}

/// Pinta los bordes de una caja. Con lados iguales y `solid` emite un único
/// `StrokeRect` (conserva el `border-radius`); en cualquier otro caso pinta
/// cada lado por separado, con guiones o puntos si el estilo lo pide.
fn paint_border(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    border: [f32; 4],
    style: &Style,
    radius: f32,
    commands: &mut Vec<PaintCommand>,
) {
    let sides: Vec<usize> = (0..4)
        .filter(|side| {
            border[*side] > 0.0
                && style.border_style[*side] != BorderStyleKind::None
                && style.border_color[*side].is_some()
        })
        .collect();
    if sides.is_empty() {
        return;
    }

    let uniform = sides.len() == 4
        && border[0] == border[1]
        && border[1] == border[2]
        && border[2] == border[3]
        && style
            .border_style
            .iter()
            .all(|kind| *kind == BorderStyleKind::Solid)
        && style.border_color.windows(2).all(|pair| pair[0] == pair[1]);
    if uniform && let Some(color) = style.border_color[0] {
        commands.push(PaintCommand::StrokeRect {
            rect: Rect {
                x,
                y,
                width,
                height,
            },
            color,
            width: border[0],
            radius,
        });
        return;
    }

    for side in sides {
        let thickness = border[side];
        let Some(color) = style.border_color[side] else {
            continue;
        };
        let horizontal = side == 0 || side == 2;
        let rect = match side {
            0 => Rect {
                x,
                y,
                width,
                height: thickness,
            },
            1 => Rect {
                x: x + width - thickness,
                y,
                width: thickness,
                height,
            },
            2 => Rect {
                x,
                y: y + height - thickness,
                width,
                height: thickness,
            },
            _ => Rect {
                x,
                y,
                width: thickness,
                height,
            },
        };
        match style.border_style[side] {
            BorderStyleKind::None => {}
            BorderStyleKind::Solid => commands.push(PaintCommand::FillRect {
                rect,
                color,
                radius: 0.0,
            }),
            BorderStyleKind::Dashed => {
                let dash = (thickness * 3.0).max(2.0);
                let gap = (thickness * 2.0).max(2.0);
                paint_border_stripes(rect, horizontal, dash, gap, color, commands);
            }
            BorderStyleKind::Dotted => {
                let dash = thickness.max(1.0);
                let gap = (thickness * 1.5).max(1.0);
                paint_border_stripes(rect, horizontal, dash, gap, color, commands);
            }
        }
    }
}

fn paint_border_stripes(
    rect: Rect,
    horizontal: bool,
    dash: f32,
    gap: f32,
    color: Color,
    commands: &mut Vec<PaintCommand>,
) {
    let length = if horizontal { rect.width } else { rect.height };
    let mut offset = 0.0;
    while offset < length {
        let segment = dash.min(length - offset);
        let stripe = if horizontal {
            Rect {
                x: rect.x + offset,
                y: rect.y,
                width: segment,
                height: rect.height,
            }
        } else {
            Rect {
                x: rect.x,
                y: rect.y + offset,
                width: rect.width,
                height: segment,
            }
        };
        commands.push(PaintCommand::FillRect {
            rect: stripe,
            color,
            radius: 0.0,
        });
        offset += dash + gap;
    }
}

/// Item de un contenedor flex/grid: un elemento del árbol o un grupo de nodos
/// en línea consecutivos (los navegadores los envuelven en "flex items"
/// anónimos; aquí se tratan como una sola caja).
enum TaffyItem<'a> {
    Run(Vec<&'a DomNode>),
    Element(&'a DomNode),
}

/// Medidas de items Taffy memoizadas durante un layout completo. La clave es
/// `(identidad del nodo, nº de nodos del run, ancho disponible en bits)`: las
/// medidas son deterministas por ancho, así que la pasada de medida y la de
/// pintado — y los reintentos internos de Taffy — reutilizan el mismo tamaño.
type MeasureCache = HashMap<(usize, usize, u32), Size<f32>>;

/// Layout de los hijos de un contenedor `display: flex` o `display: grid`.
/// Cada item se convierte en una hoja de Taffy cuya medida se obtiene
/// ejecutando el layout habitual de Wisp sobre una lista de pintado
/// desechable; Taffy resuelve posiciones, gaps, grow/shrink y alineación, y
/// después cada subárbol se pinta en su rectángulo final.
fn layout_taffy_children(
    node: &DomNode,
    x: f32,
    y: f32,
    width: f32,
    parent_style: Style,
    images: &HashMap<String, ImageResource>,
    commands: &mut Vec<PaintCommand>,
    depth: usize,
    measure_only: bool,
    measure_cache: &mut MeasureCache,
) -> Result<f32> {
    ensure!(
        depth <= MAX_TREE_DEPTH,
        "layout nesting exceeds Wisp's depth limit"
    );

    let mut items: Vec<TaffyItem<'_>> = Vec::new();
    let mut run: Vec<&DomNode> = Vec::new();
    let mut absolute_children: Vec<&DomNode> = Vec::new();
    // En un grid todo hijo es un item ("blockificado"): un `<span>` con
    // `grid-column` tiene que ocupar pistas, no fluir como texto inline.
    let grid_container = matches!(parent_style.display, Display::Grid);
    for child in &node.children {
        if is_hidden(child) {
            continue;
        }
        if cached_style(child, parent_style).position == PositionKind::Absolute {
            absolute_children.push(child);
            continue;
        }
        if is_block(child) || (grid_container && child.tag.is_some()) {
            if !run.is_empty() {
                items.push(TaffyItem::Run(std::mem::take(&mut run)));
            }
            items.push(TaffyItem::Element(child));
        } else {
            run.push(child);
        }
    }
    if !run.is_empty() {
        items.push(TaffyItem::Run(run));
    }
    if items.is_empty() {
        if !measure_only {
            for child in absolute_children {
                layout_absolute_child(
                    child,
                    parent_style,
                    x,
                    y,
                    width,
                    0.0,
                    images,
                    commands,
                    depth,
                    false,
                    measure_cache,
                )?;
            }
        }
        return Ok(0.0);
    }

    let mut taffy: TaffyTree<usize> = TaffyTree::new();
    let mut nodes = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let style = taffy_item_style(item, parent_style, width);
        let node_id = taffy
            .new_leaf_with_context(style, index)
            .map_err(|error| anyhow::anyhow!("taffy leaf failed: {error}"))?;
        nodes.push(node_id);
    }
    let root_style = taffy_container_style(node, parent_style, width);
    let root = taffy
        .new_with_children(root_style, &nodes)
        .map_err(|error| anyhow::anyhow!("taffy root failed: {error}"))?;

    let available_space = Size {
        width: AvailableSpace::Definite(width),
        height: AvailableSpace::MaxContent,
    };
    let container_width = width;
    taffy
        .compute_layout_with_measure(root, available_space, |inputs, _node_id, context, style| {
            let index = context.map(|context| *context).unwrap_or(0);
            taffy::compute_leaf_layout(
                inputs,
                style,
                |_, _| 0.0,
                |known, available| {
                    measure_taffy_item(
                        &items[index],
                        known,
                        available,
                        container_width,
                        parent_style,
                        images,
                        depth + 1,
                        measure_cache,
                    )
                },
            )
        })
        .map_err(|error| anyhow::anyhow!("taffy compute failed: {error}"))?;

    let height = taffy
        .layout(root)
        .map_err(|error| anyhow::anyhow!("taffy layout failed: {error}"))?
        .size
        .height;
    if measure_only {
        return Ok(height.max(0.0));
    }
    for (index, item) in items.iter().enumerate() {
        let layout = taffy
            .layout(nodes[index])
            .map_err(|error| anyhow::anyhow!("taffy layout failed: {error}"))?;
        let item_x = x + layout.location.x;
        let item_y = y + layout.location.y;
        let item_width = layout.size.width.max(1.0);
        match item {
            TaffyItem::Element(child) => {
                let (offset_x, offset_y) =
                    relative_offset(cached_style(child, parent_style), item_width);
                layout_block(
                    child,
                    item_x + offset_x,
                    item_y + offset_y,
                    item_width,
                    parent_style,
                    images,
                    commands,
                    depth + 1,
                    false,
                    measure_cache,
                )?;
            }
            TaffyItem::Run(children) => {
                let mut run_items = Vec::new();
                for child in children {
                    collect_inline_runs(child, parent_style, None, images, &mut run_items);
                }
                let (_, _) = flush_inline_runs(
                    &mut run_items,
                    item_x,
                    item_y,
                    item_width,
                    images,
                    commands,
                    parent_style.text_align,
                    false,
                );
            }
        }
    }
    for child in absolute_children {
        layout_absolute_child(
            child,
            parent_style,
            x,
            y,
            width,
            height.max(0.0),
            images,
            commands,
            depth,
            false,
            measure_cache,
        )?;
    }
    Ok(height.max(0.0))
}

/// Medida de un item de Taffy: ejecuta el layout normal de Wisp sobre una
/// lista de pintado desechable y devuelve el tamaño de la caja resultante.
fn measure_taffy_item(
    item: &TaffyItem<'_>,
    known_dimensions: Size<Option<f32>>,
    available_space: Size<AvailableSpace>,
    container_width: f32,
    parent_style: Style,
    images: &HashMap<String, ImageResource>,
    depth: usize,
    measure_cache: &mut MeasureCache,
) -> Size<f32> {
    let layout_width = known_dimensions
        .width
        .or(match available_space.width {
            AvailableSpace::Definite(width) => Some(width),
            _ => None,
        })
        .unwrap_or(container_width)
        .max(1.0);

    let (key_node, key_len) = match item {
        TaffyItem::Element(child) => (*child as *const DomNode as usize, 0),
        TaffyItem::Run(children) => (children[0] as *const DomNode as usize, children.len()),
    };
    let key = (key_node, key_len, layout_width.to_bits());
    if let Some(size) = measure_cache.get(&key) {
        return *size;
    }

    let size = match item {
        TaffyItem::Element(child) => {
            let mut scratch = Vec::new();
            let height = layout_block(
                child,
                0.0,
                0.0,
                layout_width,
                parent_style,
                images,
                &mut scratch,
                depth + 1,
                true,
                measure_cache,
            )
            .unwrap_or(0.0);
            let child_style = cached_style(child, parent_style);
            let margin_x = child_style.margin[1].resolve(layout_width)
                + child_style.margin[3].resolve(layout_width);
            let outer_width = outer_width_for(child_style, layout_width);
            Size {
                width: (margin_x + outer_width).max(0.0),
                height,
            }
        }
        TaffyItem::Run(children) => {
            let mut run_items = Vec::new();
            for child in children {
                collect_inline_runs(child, parent_style, None, images, &mut run_items);
            }
            let (height, width) = flush_inline_runs(
                &mut run_items,
                0.0,
                0.0,
                layout_width,
                images,
                &mut Vec::new(),
                parent_style.text_align,
                true,
            );
            Size { width, height }
        }
    };
    measure_cache.insert(key, size);
    size
}

/// Estilo de Taffy para un item: los elementos heredan tamaño y propiedades
/// flex de su estilo calculado; los runs de texto usan el tamaño automático.
fn taffy_item_style(
    item: &TaffyItem<'_>,
    parent_style: Style,
    container_width: f32,
) -> taffy::Style {
    let mut style = taffy::Style::default();
    if let TaffyItem::Element(child) = item {
        let child_style = cached_style(child, parent_style);
        let dimension = |length: Option<Length>| {
            length
                .map(|length| taffy_dimension(length, container_width))
                .unwrap_or(Dimension::AUTO)
        };
        style.size = Size {
            width: dimension(child_style.width),
            height: dimension(child_style.height),
        };
        let min_max = |length: Option<Length>| {
            length
                .map(|length| taffy_length_percentage_auto(length, container_width))
                .unwrap_or(LengthPercentageAuto::AUTO)
        };
        style.min_size = Size {
            width: min_max(child_style.min_width),
            height: min_max(child_style.min_height),
        };
        style.max_size = Size {
            width: min_max(child_style.max_width),
            height: min_max(child_style.max_height),
        };
        style.flex_basis = dimension(child_style.flex_basis);
        style.flex_grow = child_style.flex_grow;
        style.flex_shrink = child_style.flex_shrink;
        style.aspect_ratio = child_style.aspect_ratio;
        if matches!(parent_style.display, Display::Grid) {
            style.grid_column = taffy_grid_line(child_style.grid_column);
            style.grid_row = taffy_grid_line(child_style.grid_row);
        }
    }
    style
}

/// `GridLinePlacement` de Wisp → `Line<GridPlacement>` de Taffy.
fn taffy_grid_line(placement: GridLinePlacement) -> taffy::geometry::Line<GridPlacement> {
    let mut line = taffy::geometry::Line::<GridPlacement>::default();
    if let Some(start) = placement.start {
        line.start = GridPlacement::Line(start.into());
    }
    if let Some(end) = placement.end {
        line.end = GridPlacement::Line(end.into());
    } else if placement.span > 0 {
        line.end = GridPlacement::Span(placement.span);
    }
    line
}

/// Estilo de Taffy para el contenedor flex/grid: traduce las propiedades CSS
/// de `Style` y, en el caso de grid, las plantillas `grid-template-*`.
fn taffy_container_style(node: &DomNode, parent_style: Style, width: f32) -> taffy::Style {
    let mut style = taffy::Style::default();
    style.display = match parent_style.display {
        Display::Grid => taffy::Display::Grid,
        _ => taffy::Display::Flex,
    };
    style.flex_direction = match parent_style.flex_direction {
        FlexDirectionKind::Row => FlexDirection::Row,
        FlexDirectionKind::RowReverse => FlexDirection::RowReverse,
        FlexDirectionKind::Column => FlexDirection::Column,
        FlexDirectionKind::ColumnReverse => FlexDirection::ColumnReverse,
    };
    style.flex_wrap = if parent_style.flex_wrap {
        FlexWrap::Wrap
    } else {
        FlexWrap::NoWrap
    };
    style.justify_content = match parent_style.justify_content {
        JustifyKind::Start => JustifyContent::START,
        JustifyKind::End => JustifyContent::END,
        JustifyKind::Center => JustifyContent::CENTER,
        JustifyKind::SpaceBetween => JustifyContent::SPACE_BETWEEN,
        JustifyKind::SpaceAround => JustifyContent::SPACE_AROUND,
        JustifyKind::SpaceEvenly => JustifyContent::SPACE_EVENLY,
    };
    style.align_items = match parent_style.align_items {
        AlignKind::Stretch => AlignItems::STRETCH,
        AlignKind::Start => AlignItems::FLEX_START,
        AlignKind::End => AlignItems::FLEX_END,
        AlignKind::Center => AlignItems::CENTER,
        AlignKind::Baseline => AlignItems::BASELINE,
    };
    style.gap = Size {
        width: taffy_length_percentage(parent_style.gap[1], width),
        height: taffy_length_percentage(parent_style.gap[0], width),
    };
    style.size = Size {
        width: Dimension::from_length(width),
        height: Dimension::AUTO,
    };
    if matches!(parent_style.display, Display::Grid) {
        if let Some(tracks) = inline_declaration(node, "grid-template-columns") {
            style.grid_template_columns = parse_grid_template(tracks);
        }
        if let Some(tracks) = inline_declaration(node, "grid-template-rows") {
            style.grid_template_rows = parse_grid_template(tracks);
        }
    }
    style
}

/// `Length` de Wisp → `Dimension` de Taffy. Las expresiones de `calc()` se
/// resuelven contra el ancho del contenedor porque Taffy no las interpreta.
fn taffy_dimension(length: Length, basis: f32) -> Dimension {
    match length {
        Length::Px(px) => Dimension::from_length(px),
        Length::Percent(percent) => Dimension::from_percent(percent / 100.0),
        Length::Sum { .. } | Length::Function { .. } => {
            Dimension::from_length(length.resolve(basis))
        }
    }
}

/// `Length` de Wisp → `LengthPercentage` de Taffy (para `gap`).
fn taffy_length_percentage(length: Length, basis: f32) -> LengthPercentage {
    match length {
        Length::Px(px) => taffy::style_helpers::length(px),
        Length::Percent(percent) => taffy::style_helpers::percent(percent / 100.0),
        Length::Sum { .. } | Length::Function { .. } => {
            taffy::style_helpers::length(length.resolve(basis))
        }
    }
}

/// `Length` de Wisp → `LengthPercentageAuto` de Taffy (para `min-*`/`max-*`).
fn taffy_length_percentage_auto(length: Length, basis: f32) -> LengthPercentageAuto {
    match length {
        Length::Px(px) => LengthPercentageAuto::length(px),
        Length::Percent(percent) => LengthPercentageAuto::percent(percent / 100.0),
        Length::Sum { .. } | Length::Function { .. } => {
            LengthPercentageAuto::length(length.resolve(basis))
        }
    }
}

/// Valor de una declaración dentro del `inline_style` ya fusionado del nodo
/// (hojas de estilo + atributo `style`).
fn inline_declaration<'a>(node: &'a DomNode, name: &str) -> Option<&'a str> {
    node.inline_style.split(';').find_map(|declaration| {
        let (property, value) = declaration.split_once(':')?;
        property
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim())
    })
}

/// Parsea `grid-template-columns`/`grid-template-rows`: longitudes `px`,
/// porcentajes, `fr`, `auto`, `minmax(a, b)` y `repeat(N | auto-fill |
/// auto-fit, ...)`.
fn parse_grid_template(value: &str) -> Vec<GridTemplateComponent<String>> {
    let mut components = Vec::new();
    // El spec admite una sola repetición automática por plantilla.
    let mut auto_repeat_used = false;
    for token in split_track_tokens(value.trim()) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some(inner) = strip_css_function(token, "repeat") {
            let Some((count_text, list)) = inner.split_once(',') else {
                continue;
            };
            let count_text = count_text.trim().to_ascii_lowercase();
            let count = match count_text.as_str() {
                "auto-fill" | "auto-fit" if !auto_repeat_used => {
                    auto_repeat_used = true;
                    if count_text == "auto-fill" {
                        RepetitionCount::AutoFill
                    } else {
                        RepetitionCount::AutoFit
                    }
                }
                _ => match count_text.parse::<u16>() {
                    Ok(count) => RepetitionCount::Count(count.min(16)),
                    Err(_) => continue,
                },
            };
            let tracks = parse_track_list(list);
            if tracks.is_empty() {
                continue;
            }
            components.push(GridTemplateComponent::Repeat(
                taffy::style::GridTemplateRepetition {
                    count,
                    tracks,
                    line_names: Vec::new(),
                },
            ));
        } else if let Some(track) = parse_track_sizing(token) {
            components.push(GridTemplateComponent::Single(track));
        }
    }
    components
}

/// Separa los componentes de una plantilla por espacios de primer nivel (sin
/// cortar dentro de `minmax(...)` ni `repeat(...)`).
fn split_track_tokens(value: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    for (index, character) in value.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            _ => {}
        }
        if character.is_whitespace() && depth == 0 {
            if let Some(start) = start.take() {
                tokens.push(&value[start..index]);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(start) = start {
        tokens.push(&value[start..]);
    }
    tokens
}

/// `nombre(args)` sin los paréntesis exteriores, insensible a mayúsculas.
fn strip_css_function<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    let value = value.trim();
    let inner = value.strip_suffix(')')?;
    let open = inner.find('(')?;
    if !inner[..open].trim().eq_ignore_ascii_case(name) {
        return None;
    }
    Some(&inner[open + 1..])
}

fn parse_track_list(value: &str) -> Vec<TrackSizingFunction> {
    split_track_tokens(value.trim())
        .into_iter()
        .filter_map(parse_track_sizing)
        .collect()
}

/// `minmax(a, b)` o una única función de tamaño (que implica `minmax(auto,
/// valor)`, salvo en longitudes fijas, donde el mínimo también es fijo).
fn parse_track_sizing(token: &str) -> Option<TrackSizingFunction> {
    if let Some(inner) = strip_css_function(token, "minmax") {
        let parts = split_top_level_commas(inner);
        if parts.len() != 2 {
            return None;
        }
        return Some(TrackSizingFunction {
            min: parse_min_track(parts[0])?,
            max: parse_max_track(parts[1])?,
        });
    }
    let max = parse_max_track(token)?;
    let min = parse_min_track(token).unwrap_or_else(MinTrackSizingFunction::auto);
    Some(TrackSizingFunction { min, max })
}

fn parse_min_track(token: &str) -> Option<MinTrackSizingFunction> {
    let token = token.trim();
    if token.eq_ignore_ascii_case("auto") {
        return Some(MinTrackSizingFunction::auto());
    }
    if let Some(percent) = token.strip_suffix('%') {
        let percent = percent.trim().parse::<f32>().ok()?;
        return (percent.is_finite() && percent >= 0.0)
            .then(|| MinTrackSizingFunction::percent(percent / 100.0));
    }
    parse_length(token).map(MinTrackSizingFunction::length)
}

fn parse_max_track(token: &str) -> Option<MaxTrackSizingFunction> {
    let token = token.trim();
    if let Some(fr) = token.strip_suffix("fr") {
        let fr = fr.trim().parse::<f32>().ok()?;
        return (fr.is_finite() && fr >= 0.0).then(|| MaxTrackSizingFunction::fr(fr));
    }
    if token.eq_ignore_ascii_case("auto") {
        return Some(MaxTrackSizingFunction::auto());
    }
    if let Some(percent) = token.strip_suffix('%') {
        let percent = percent.trim().parse::<f32>().ok()?;
        return (percent.is_finite() && percent >= 0.0)
            .then(|| MaxTrackSizingFunction::percent(percent / 100.0));
    }
    parse_length(token).map(MaxTrackSizingFunction::length)
}

/// `aspect-ratio: 16 / 9`, `1.5` o `auto`.
fn parse_aspect_ratio(value: &str) -> Option<f32> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("auto") {
        return None;
    }
    let ratio = match value.split_once('/') {
        Some((width, height)) => {
            let width = width.trim().parse::<f32>().ok()?;
            let height = height.trim().parse::<f32>().ok()?;
            (height != 0.0).then(|| width / height)?
        }
        None => value.parse::<f32>().ok()?,
    };
    (ratio.is_finite() && ratio > 0.0).then_some(ratio)
}

fn parse_grid_line(value: &str) -> Option<Option<i16>> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("auto") {
        return Some(None);
    }
    value.parse::<i16>().ok().map(Some)
}

/// `grid-column`/`grid-row`: `2`, `1 / 3`, `span 2`, `2 / span 3`.
fn parse_grid_placement(value: &str) -> Option<GridLinePlacement> {
    let mut parts = value.split('/');
    let start = parts.next().unwrap_or_default().trim();
    let end = parts.next().map(str::trim);
    let mut placement = GridLinePlacement::default();
    apply_grid_placement(&mut placement, start, true)?;
    if let Some(end) = end {
        apply_grid_placement(&mut placement, end, false)?;
    }
    Some(placement)
}

fn apply_grid_placement(
    placement: &mut GridLinePlacement,
    value: &str,
    is_start: bool,
) -> Option<()> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("auto") {
        return Some(());
    }
    let lower = value.to_ascii_lowercase();
    if let Some(span) = lower.strip_prefix("span") {
        let span = span.trim().parse::<u16>().ok()?;
        placement.span = span.max(1);
        return Some(());
    }
    let line = value.parse::<i16>().ok()?;
    if is_start {
        placement.start = Some(line);
    } else {
        placement.end = Some(line);
    }
    Some(())
}

/// Shorthand `border`: acepta `<ancho> <estilo> <color>` en cualquier orden
/// (`1px solid red`, `solid 2px #00ff00`…) y lo aplica a los cuatro lados.
fn parse_border_shorthand(value: &str, style: &mut Style) {
    let mut width = style.border_width;
    let mut colors = style.border_color;
    let mut styles = style.border_style;
    let Some((side_width, side_color, side_style)) = parse_border_tokens(value, style.font_size)
    else {
        return;
    };
    let width_value = side_width.or_else(|| {
        // `border: solid red` sin ancho usa el `medium` de CSS (3 px).
        side_style
            .filter(|kind| *kind != BorderStyleKind::None)
            .map(|_| Length::Px(3.0))
    });
    if let Some(width_value) = width_value {
        width = [width_value; 4];
    }
    if let Some(color) = side_color {
        colors = [Some(color); 4];
    }
    if let Some(kind) = side_style {
        styles = [kind; 4];
    }
    style.border_width = width;
    style.border_color = colors;
    style.border_style = styles;
}

/// Shorthand por lado (`border-top`, `border-left`…).
fn parse_border_side(value: &str, style: &mut Style, side: usize) {
    let Some((width, color, kind)) = parse_border_tokens(value, style.font_size) else {
        return;
    };
    if let Some(width) = width {
        style.border_width[side] = width;
    }
    if let Some(color) = color {
        style.border_color[side] = Some(color);
    }
    if let Some(kind) = kind {
        style.border_style[side] = kind;
    }
}

/// Interpreta los tokens de un shorthand de borde. Devuelve `None` cuando
/// ningún token es reconocible para no borrar un borde ya declarado.
fn parse_border_tokens(
    value: &str,
    font_size: f32,
) -> Option<(Option<Length>, Option<Color>, Option<BorderStyleKind>)> {
    let mut width = None;
    let mut color = None;
    let mut kind = None;
    let mut recognized = false;
    for token in split_css_components(value) {
        if let Some(length) = parse_dimension_em(token, font_size) {
            width = Some(length);
            recognized = true;
        } else if let Some(parsed) = parse_border_style(token) {
            kind = Some(parsed);
            recognized = true;
        } else if let Some(parsed) = parse_color(token) {
            color = Some(parsed);
            recognized = true;
        }
    }
    recognized.then_some((width, color, kind))
}

fn parse_border_style(token: &str) -> Option<BorderStyleKind> {
    Some(match token.trim().to_ascii_lowercase().as_str() {
        "none" | "hidden" => BorderStyleKind::None,
        "solid" | "double" | "groove" | "ridge" | "inset" | "outset" => BorderStyleKind::Solid,
        "dashed" => BorderStyleKind::Dashed,
        "dotted" => BorderStyleKind::Dotted,
        _ => return None,
    })
}

fn set_border_style(style: &mut Style, side: usize, value: &str) {
    if let Some(kind) = parse_border_style(value) {
        style.border_style[side] = kind;
    }
}

fn set_border_color(style: &mut Style, side: usize, value: &str) {
    if let Some(color) = parse_color(value) {
        style.border_color[side] = Some(color);
    }
}

/// `border-color` con 1–4 valores (arriba, derecha, abajo, izquierda).
fn set_border_colors(style: &mut Style, value: &str) {
    let colors = split_css_components(value)
        .into_iter()
        .map(parse_color)
        .collect::<Option<Vec<_>>>()
        .and_then(expand_box_values);
    if let Some(colors) = colors {
        style.border_color = colors.map(Some);
    }
}

/// `border-style` con 1–4 valores.
fn set_border_styles(style: &mut Style, value: &str) {
    let styles = split_css_components(value)
        .into_iter()
        .map(parse_border_style)
        .collect::<Option<Vec<_>>>()
        .and_then(expand_box_values);
    if let Some(styles) = styles {
        style.border_style = styles;
    }
}

fn parse_gap_em(value: &str, font_size: f32) -> Option<[Length; 2]> {
    let lengths = split_css_components(value)
        .into_iter()
        .map(|token| parse_dimension_em(token, font_size))
        .collect::<Option<Vec<_>>>()?;
    match lengths.len() {
        1 => Some([lengths[0], lengths[0]]),
        2 => Some([lengths[0], lengths[1]]),
        _ => None,
    }
}

/// Estado de una lista de pintado en línea: acumula comandos por línea,
/// decide los saltos y recuerda el alto de cada caja de línea para centrar el
/// texto cuando hay `line-height`. También aplica `white-space`,
/// `text-transform` y el subrayado de `text-decoration`.
struct InlineWriter<'a> {
    commands: &'a mut Vec<PaintCommand>,
    /// `(primer comando de la línea, alto de su caja)`.
    lines: Vec<(usize, f32)>,
    line_start: usize,
    x: f32,
    width: f32,
    y: f32,
    start_y: f32,
    line_width: f32,
    line_height: f32,
    has_content: bool,
    pending_space: bool,
    max_line_width: f32,
    measure_only: bool,
}

impl<'a> InlineWriter<'a> {
    fn new(
        commands: &'a mut Vec<PaintCommand>,
        x: f32,
        y: f32,
        width: f32,
        measure_only: bool,
    ) -> Self {
        let line_start = commands.len();
        Self {
            commands,
            lines: Vec::new(),
            line_start,
            x,
            width: width.max(1.0),
            y,
            start_y: y,
            line_width: 0.0,
            line_height: 0.0,
            has_content: false,
            pending_space: false,
            max_line_width: 0.0,
            measure_only,
        }
    }

    /// Alto de la caja de línea que ocupa `style` (`line-height` incluido).
    fn line_box(&self, style: Style) -> f32 {
        let natural = text_line_height(style.font_size);
        style
            .line_height
            .map_or(natural, |height| height.max(natural))
    }

    fn space_before(&self, space_before: bool, style: Style) -> f32 {
        if space_before && self.has_content {
            text_width_weighted(" ", style.font_size, is_bold_weight(style.font_weight))
        } else {
            0.0
        }
    }

    fn break_line(&mut self, box_height: f32) {
        self.y += box_height.max(0.0);
        self.lines.push((self.line_start, box_height.max(0.0)));
        self.line_start = self.commands.len();
        self.line_width = 0.0;
        self.line_height = 0.0;
        self.has_content = false;
        self.pending_space = false;
    }

    /// Salto duro: `<br>` o `\n` dentro de `white-space: pre*`.
    fn hard_break(&mut self, style: Style) {
        let box_height = if self.has_content {
            self.line_height
        } else {
            self.line_box(style).max(self.line_height)
        };
        self.break_line(box_height);
    }

    fn place_span(
        &mut self,
        content: &str,
        style: Style,
        href: Option<&str>,
        space_before: bool,
        wrap: bool,
    ) {
        let content = transformed_text(content, style.text_transform);
        let word_width =
            text_width_weighted(&content, style.font_size, is_bold_weight(style.font_weight));
        let space_width = self.space_before(space_before, style);
        if wrap && self.has_content && self.line_width + space_width + word_width > self.width {
            let box_height = self.line_height.max(self.line_box(style));
            self.break_line(box_height);
        }
        let space_width = self.space_before(space_before, style);
        let x = self.x + self.line_width + space_width;
        let y = self.y;
        if !self.measure_only && style.visibility == VisibilityKind::Visible {
            self.commands.push(PaintCommand::Text {
                x,
                y,
                content: content.into_owned(),
                color: style.color,
                font_size: style.font_size,
                font_weight: style.font_weight,
                href: href.map(str::to_owned),
            });
            if style.underline {
                self.commands.push(PaintCommand::FillRect {
                    rect: Rect {
                        x,
                        y: y + text_ascent(style.font_size) + 1.0,
                        width: word_width.max(1.0),
                        height: 1.0,
                    },
                    color: style.color,
                    radius: 0.0,
                });
            }
            if style.line_through {
                self.commands.push(PaintCommand::FillRect {
                    rect: Rect {
                        x,
                        y: y + text_ascent(style.font_size) * 0.55,
                        width: word_width.max(1.0),
                        height: 1.0,
                    },
                    color: style.color,
                    radius: 0.0,
                });
            }
        }
        self.line_width += space_width + word_width;
        self.max_line_width = self.max_line_width.max(self.line_width);
        self.line_height = self.line_height.max(self.line_box(style));
        self.has_content = true;
    }

    fn place_image(
        &mut self,
        src: &str,
        image_width: f32,
        image_height: f32,
        style: Style,
        href: Option<&str>,
    ) {
        let mut space_width = self.space_before(self.pending_space, style);
        if self.has_content && self.line_width + space_width + image_width > self.width {
            let box_height = self.line_height.max(self.line_box(style));
            self.break_line(box_height);
            space_width = 0.0;
        }
        if !self.measure_only && style.visibility == VisibilityKind::Visible {
            self.commands.push(PaintCommand::Image {
                rect: Rect {
                    x: self.x + self.line_width + space_width,
                    y: self.y,
                    width: image_width,
                    height: image_height,
                },
                url: src.to_owned(),
                href: href.map(str::to_owned),
            });
        }
        self.line_width += space_width + image_width;
        self.max_line_width = self.max_line_width.max(self.line_width);
        self.line_height = self.line_height.max(image_height);
        self.has_content = true;
        self.pending_space = false;
    }

    /// Coloca una palabra que no cabe en una línea partiéndola por
    /// caracteres (`overflow-wrap: break-word`).
    fn place_broken_word(
        &mut self,
        word: &str,
        style: Style,
        href: Option<&str>,
        space_before: bool,
    ) {
        let mut remaining = word;
        let mut first = true;
        while !remaining.is_empty() {
            let mut end = 0usize;
            let mut width = 0.0f32;
            for (index, character) in remaining.char_indices() {
                let next = index + character.len_utf8();
                let character_width = text_width_weighted(
                    &remaining[index..next],
                    style.font_size,
                    is_bold_weight(style.font_weight),
                );
                if end > 0 && width + character_width > self.width {
                    break;
                }
                width += character_width;
                end = next;
            }
            if end == 0 {
                end = remaining.len();
            }
            self.place_span(&remaining[..end], style, href, first && space_before, true);
            remaining = &remaining[end..];
            first = false;
        }
    }

    /// Cierra la última línea, centra el texto dentro de su caja y aplica
    /// `text-align`. Devuelve `(alto total, ancho máximo de línea)`.
    fn finish(mut self, align: TextAlignKind) -> (f32, f32) {
        if self.has_content {
            let box_height = self.line_height;
            self.break_line(box_height);
        }
        let height = self.y - self.start_y;
        if !self.measure_only {
            center_line_boxes(self.commands, &self.lines);
            if align != TextAlignKind::Left {
                align_inline_lines(self.commands, &self.lines, self.x, self.width, align);
            }
        }
        (height, self.max_line_width)
    }
}

/// Centra verticalmente las palabras dentro de su caja de línea: con
/// `line-height` mayor que el alto natural, el espacio sobrante se reparte
/// arriba y abajo como en un navegador.
fn center_line_boxes(commands: &mut [PaintCommand], lines: &[(usize, f32)]) {
    for (index, (start, box_height)) in lines.iter().enumerate() {
        let end = lines
            .get(index + 1)
            .map(|line| line.0)
            .unwrap_or(commands.len());
        if end <= *start {
            continue;
        }
        let mut last_text_offset = 0.0;
        for command in &mut commands[*start..end] {
            match command {
                PaintCommand::Text { y, font_size, .. } => {
                    let natural = text_line_height(*font_size);
                    let offset = ((*box_height - natural) / 2.0).max(0.0);
                    last_text_offset = offset;
                    if offset > 0.01 {
                        *y += offset;
                    }
                }
                // El subrayado sigue al texto al que pertenece.
                PaintCommand::FillRect { rect, .. } if last_text_offset > 0.01 => {
                    rect.y += last_text_offset;
                }
                _ => {}
            }
        }
    }
}

/// Reparte las palabras de `text` en el escritor, colapsando los espacios
/// según CSS y respetando `wrap` (`white-space: nowrap` no rompe líneas).
fn flow_words(
    writer: &mut InlineWriter<'_>,
    text: &str,
    style: Style,
    href: Option<&str>,
    wrap: bool,
) {
    // Corta por índices de byte en lugar de acumular en un `String`: así cada
    // palabra es una rebanada de `text` y no hay una asignación por palabra
    // (ni un `push` por carácter) en el camino caliente.
    let mut word_start: Option<usize> = None;
    for (index, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = word_start.take() {
                place_flow_word(writer, &text[start..index], style, href, wrap);
            }
            writer.pending_space = true;
        } else if word_start.is_none() {
            word_start = Some(index);
        }
    }
    if let Some(start) = word_start {
        place_flow_word(writer, &text[start..], style, href, wrap);
        writer.pending_space = false;
    }
}

fn place_flow_word(
    writer: &mut InlineWriter<'_>,
    word: &str,
    style: Style,
    href: Option<&str>,
    wrap: bool,
) {
    let space = writer.pending_space;
    let too_wide = wrap
        && style.overflow_wrap
        && text_width_weighted(
            &transformed_text(word, style.text_transform),
            style.font_size,
            is_bold_weight(style.font_weight),
        ) > writer.width;
    if too_wide {
        writer.place_broken_word(word, style, href, space);
    } else {
        writer.place_span(word, style, href, space, wrap);
    }
}

/// Devuelve `(altura, ancho máximo de línea)`. Con `measure_only` no emite
/// comandos: solo calcula el espacio que ocuparía el texto.
fn flush_inline_runs(
    items: &mut Vec<InlineItem<'_>>,
    x: f32,
    y: f32,
    width: f32,
    images: &HashMap<String, ImageResource>,
    commands: &mut Vec<PaintCommand>,
    align: TextAlignKind,
    measure_only: bool,
) -> (f32, f32) {
    let mut writer = InlineWriter::new(commands, x, y, width, measure_only);
    let mut last_style = Style::default();
    for item in items.iter() {
        match item {
            InlineItem::Text { text, style, href } => {
                last_style = *style;
                match style.white_space {
                    // `pre` conserva espacios y saltos y no ajusta líneas.
                    WhiteSpaceKind::Pre => {
                        for (index, segment) in text.split('\n').enumerate() {
                            if index > 0 {
                                writer.hard_break(*style);
                            }
                            if segment.is_empty() {
                                continue;
                            }
                            writer.place_span(segment, *style, *href, false, false);
                        }
                    }
                    // `pre-wrap` conserva los saltos duros y ajusta palabras.
                    WhiteSpaceKind::PreWrap => {
                        for (index, segment) in text.split('\n').enumerate() {
                            if index > 0 {
                                writer.hard_break(*style);
                            }
                            flow_words(&mut writer, segment, *style, *href, true);
                        }
                    }
                    WhiteSpaceKind::Normal | WhiteSpaceKind::NoWrap => {
                        let wrap = matches!(style.white_space, WhiteSpaceKind::Normal);
                        flow_words(&mut writer, text, *style, *href, wrap);
                    }
                }
            }
            InlineItem::Break => writer.hard_break(last_style),
            InlineItem::Image {
                src,
                width: attr_width,
                height: attr_height,
                style,
                href,
            } => {
                let Some((image_width, image_height)) =
                    image_display_size(src, *attr_width, *attr_height, *style, images, width)
                else {
                    continue;
                };
                writer.place_image(src, image_width, image_height, *style, *href);
            }
        }
    }
    items.clear();
    writer.finish(align)
}

/// Desplaza horizontalmente cada línea del rango emitido por
/// [`flush_inline_runs`] según `text-align` (center/right).
fn align_inline_lines(
    commands: &mut [PaintCommand],
    lines: &[(usize, f32)],
    x: f32,
    width: f32,
    align: TextAlignKind,
) {
    for (index, (start, _)) in lines.iter().enumerate() {
        let end = lines
            .get(index + 1)
            .map(|line| line.0)
            .unwrap_or(commands.len());
        if end <= *start {
            continue;
        }
        let mut right_edge = 0.0f32;
        for command in &commands[*start..end] {
            let right = match command {
                PaintCommand::Text {
                    x: cx,
                    content,
                    font_size,
                    font_weight,
                    ..
                } => cx + text_width_weighted(content, *font_size, is_bold_weight(*font_weight)),
                PaintCommand::Image { rect, .. }
                | PaintCommand::FillRect { rect, .. }
                | PaintCommand::StrokeRect { rect, .. }
                | PaintCommand::BackgroundImage { rect, .. }
                | PaintCommand::Clip { rect } => rect.x + rect.width,
                PaintCommand::ClipEnd => 0.0,
            };
            right_edge = right_edge.max(right);
        }
        let line_width = (right_edge - x).max(0.0);
        let delta = match align {
            TextAlignKind::Center => (width - line_width) / 2.0,
            TextAlignKind::Right => width - line_width,
            TextAlignKind::Left => 0.0,
        };
        if delta.abs() <= 0.01 {
            continue;
        }
        for command in &mut commands[*start..end] {
            match command {
                PaintCommand::Text { x: cx, .. } => *cx += delta,
                PaintCommand::Image { rect, .. }
                | PaintCommand::FillRect { rect, .. }
                | PaintCommand::StrokeRect { rect, .. }
                | PaintCommand::BackgroundImage { rect, .. }
                | PaintCommand::Clip { rect } => {
                    rect.x += delta;
                }
                PaintCommand::ClipEnd => {}
            }
        }
    }
}

/// Computes the painted size of an inline image: CSS `width`/`height` win,
/// then the `width`/`height` attributes, then the intrinsic size, preserving
/// the aspect ratio when only one dimension is given. The result is clamped
/// to the available line width.
fn image_display_size(
    src: &str,
    attr_width: Option<f32>,
    attr_height: Option<f32>,
    style: Style,
    images: &HashMap<String, ImageResource>,
    available_width: f32,
) -> Option<(f32, f32)> {
    let resource = images.get(src)?;
    let (intrinsic_width, intrinsic_height) = if resource.width > 0 && resource.height > 0 {
        (resource.width as f32, resource.height as f32)
    } else {
        // Browsers fall back to a 3:2 replaced-element box without intrinsic size.
        (300.0, 150.0)
    };
    let width = style
        .width
        .map(|length| length.resolve(available_width))
        .or(attr_width);
    let height = style
        .height
        .map(|length| length.resolve(available_width))
        .or(attr_height);
    let (mut display_width, mut display_height) = match (width, height) {
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => (width, width * intrinsic_height / intrinsic_width),
        (None, Some(height)) => (height * intrinsic_width / intrinsic_height, height),
        (None, None) => (intrinsic_width, intrinsic_height),
    };
    let max_width = available_width.max(1.0);
    if display_width > max_width {
        let scale = max_width / display_width;
        display_width = max_width;
        display_height *= scale;
    }
    Some((display_width.max(1.0), display_height.max(1.0)))
}

fn collect_inline_runs<'a>(
    node: &'a DomNode,
    inherited: Style,
    inherited_href: Option<&'a str>,
    images: &HashMap<String, ImageResource>,
    output: &mut Vec<InlineItem<'a>>,
) {
    if node.tag.is_none() {
        output.push(InlineItem::Text {
            text: &node.text,
            style: inherited,
            href: inherited_href,
        });
        return;
    }
    if is_hidden(node) {
        return;
    }
    if node.tag.as_deref() == Some("br") {
        output.push(InlineItem::Break);
        return;
    }
    let style = cached_style(node, inherited);
    let href = node.href.as_deref().or(inherited_href);
    if let Some(image) = &node.image {
        if images.contains_key(&image.src) {
            output.push(InlineItem::Image {
                src: image.src.clone(),
                width: image.width,
                height: image.height,
                style,
                href,
            });
        } else if !image.alt.trim().is_empty() {
            output.push(InlineItem::Text {
                text: image.alt.as_str(),
                style,
                href,
            });
        }
        return;
    }
    for child in &node.children {
        collect_inline_runs(child, style, href, images, output);
    }
}

fn is_block(node: &DomNode) -> bool {
    // Una vez calculado el estilo, `display` ya incorpora la hoja de estilos
    // y el atributo `style`; reescanear `inline_style` en cada llamada (esto
    // se ejecuta para cada nodo en cada pase de Taffy) es trabajo repetido.
    if let Some(style) = node.style_cache.get() {
        return matches!(
            style.display,
            Display::Block | Display::Flex | Display::Grid
        );
    }
    let Some(tag) = node.tag.as_deref() else {
        return false;
    };
    if let Some(display) = inline_display(node) {
        return matches!(display, Display::Block | Display::Flex | Display::Grid);
    }
    matches!(
        tag,
        "html"
            | "body"
            | "div"
            | "main"
            | "header"
            | "footer"
            | "nav"
            | "section"
            | "article"
            | "aside"
            | "p"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "li"
            | "pre"
            | "blockquote"
            | "hr"
            | "table"
            | "tr"
            | "td"
            | "th"
    )
}

fn is_hidden(node: &DomNode) -> bool {
    if let Some(style) = node.style_cache.get() {
        return style.display == Display::None;
    }
    inline_display(node) == Some(Display::None)
}

fn inline_display(node: &DomNode) -> Option<Display> {
    node.inline_style.split(';').find_map(|declaration| {
        let (name, value) = declaration.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case("display") {
            return None;
        }
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Display::None),
            "inline" | "inline-block" => Some(Display::Inline),
            "block" => Some(Display::Block),
            "flex" | "inline-flex" => Some(Display::Flex),
            "grid" | "inline-grid" => Some(Display::Grid),
            _ => None,
        }
    })
}

/// [`compute_style`] memoized per node. A node's computed style only depends
/// on its own declarations and its ancestors' styles — never on the viewport
/// width — so the first computation (during measure or paint) serves every
/// later pass, resize and Taffy re-measure for the life of the document.
fn cached_style(node: &DomNode, inherited: Style) -> Style {
    *node
        .style_cache
        .get_or_init(|| compute_style(node, inherited))
}

/// Declaraciones de un bloque de estilos, ya normalizadas y con su marca de
/// `!important`.
fn css_declarations(style: &str) -> Vec<(String, String, bool)> {
    style
        .split(';')
        .filter_map(|declaration| {
            let (name, value) = declaration.split_once(':')?;
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                return None;
            }
            let (value, important) = strip_important(value);
            Some((name, value.trim().to_owned(), important))
        })
        .collect()
}

/// Separa `!important` del valor de una declaración.
fn strip_important(value: &str) -> (&str, bool) {
    let value = value.trim_end();
    if let Some(index) = value.rfind('!')
        && value[index + 1..].trim().eq_ignore_ascii_case("important")
    {
        return (value[..index].trim_end(), true);
    }
    (value, false)
}

fn compute_style(node: &DomNode, inherited: Style) -> Style {
    let mut style = inherited;
    style.background = None;
    style.margin = [Length::Px(0.0); 4];
    style.margin_auto = [false; 4];
    style.padding = [Length::Px(0.0); 4];
    style.width = None;
    style.height = None;
    style.min_width = None;
    style.max_width = None;
    style.min_height = None;
    style.max_height = None;
    style.position = PositionKind::Static;
    style.insets = [None; 4];
    style.box_sizing = BoxSizingKind::ContentBox;
    style.border_width = [Length::Px(0.0); 4];
    style.border_color = [None; 4];
    style.border_radius = Length::Px(0.0);
    style.border_style = [BorderStyleKind::None; 4];
    style.flex_direction = FlexDirectionKind::Row;
    style.flex_wrap = false;
    style.justify_content = JustifyKind::Start;
    style.align_items = AlignKind::Stretch;
    style.gap = [Length::Px(0.0); 2];
    style.flex_grow = 0.0;
    style.flex_shrink = 1.0;
    style.flex_basis = None;
    style.grid_column = GridLinePlacement::default();
    style.grid_row = GridLinePlacement::default();
    style.aspect_ratio = None;
    style.overflow_clip = false;
    style.display = if is_block(node) {
        Display::Block
    } else {
        Display::Inline
    };

    if let Some(tag) = node.tag.as_deref() {
        match tag {
            "body" => style.margin = [Length::Px(8.0); 4],
            "p" => style.margin[2] = Length::Px(16.0),
            "h1" => {
                style.font_size = 32.0;
                style.font_weight = 700;
                style.margin[2] = Length::Px(12.0);
            }
            "h2" => {
                style.font_size = 24.0;
                style.font_weight = 700;
                style.margin[2] = Length::Px(10.0);
            }
            "h3" => {
                style.font_size = 20.0;
                style.font_weight = 700;
                style.margin[2] = Length::Px(8.0);
            }
            "h4" => {
                style.font_size = 18.0;
                style.font_weight = 700;
            }
            "h5" => {
                style.font_size = 16.0;
                style.font_weight = 700;
            }
            "h6" => {
                style.font_size = 14.0;
                style.font_weight = 700;
            }
            "li" => style.margin[2] = Length::Px(4.0),
            "ul" | "ol" => {
                style.padding[3] = Length::Px(40.0);
                style.margin[0] = Length::Px(16.0);
                style.margin[2] = Length::Px(16.0);
                style.list_style = if tag == "ol" {
                    ListStyleKind::Decimal
                } else {
                    ListStyleKind::Disc
                };
            }
            "b" | "strong" => style.font_weight = 700,
            "pre" => {
                style.white_space = WhiteSpaceKind::Pre;
                style.margin[2] = Length::Px(12.0);
            }
            "hr" => {
                style.border_width[0] = Length::Px(1.0);
                style.border_style[0] = BorderStyleKind::Solid;
                style.border_color[0] = Some(Color {
                    r: 200,
                    g: 200,
                    b: 200,
                    a: 255,
                });
                style.margin[0] = Length::Px(8.0);
                style.margin[2] = Length::Px(8.0);
            }
            "a" => {
                style.color = Color {
                    r: 0,
                    g: 102,
                    b: 204,
                    a: 255,
                };
                style.underline = true;
            }
            _ => {}
        }
    }

    // Las declaraciones `!important` se aplican después de las normales, así
    // que ganan aunque una regla más específica venga más tarde en la cascada.
    let mut declarations = css_declarations(&node.inline_style);
    declarations.sort_by_key(|(_, _, important)| *important);

    // `em` y `%` se resuelven contra el `font-size` del elemento, así que se
    // aplica primero sin importar el orden de las declaraciones del bloque.
    for important_pass in [false, true] {
        for (name, value, important) in &declarations {
            if *important != important_pass || !name.eq_ignore_ascii_case("font-size") {
                continue;
            }
            if let Some(size) = parse_font_size(value, inherited.font_size) {
                style.font_size = size.clamp(1.0, 256.0);
            }
        }
    }

    for (name, value, _) in &declarations {
        match name.as_str() {
            "display" => {
                style.display = match value.to_ascii_lowercase().as_str() {
                    "none" => Display::None,
                    "inline" | "inline-block" => Display::Inline,
                    "block" => Display::Block,
                    "flex" | "inline-flex" => Display::Flex,
                    "grid" | "inline-grid" => Display::Grid,
                    _ => style.display,
                };
            }
            "flex-direction" => {
                style.flex_direction = match value.to_ascii_lowercase().as_str() {
                    "row-reverse" => FlexDirectionKind::RowReverse,
                    "column" => FlexDirectionKind::Column,
                    "column-reverse" => FlexDirectionKind::ColumnReverse,
                    _ => FlexDirectionKind::Row,
                };
            }
            "flex-wrap" => style.flex_wrap = value.eq_ignore_ascii_case("wrap"),
            "justify-content" => {
                style.justify_content = match value.to_ascii_lowercase().as_str() {
                    "flex-end" | "end" | "right" => JustifyKind::End,
                    "center" => JustifyKind::Center,
                    "space-between" => JustifyKind::SpaceBetween,
                    "space-around" => JustifyKind::SpaceAround,
                    "space-evenly" => JustifyKind::SpaceEvenly,
                    _ => JustifyKind::Start,
                };
            }
            "align-items" => {
                style.align_items = match value.to_ascii_lowercase().as_str() {
                    "flex-start" | "start" => AlignKind::Start,
                    "flex-end" | "end" => AlignKind::End,
                    "center" => AlignKind::Center,
                    "baseline" => AlignKind::Baseline,
                    _ => AlignKind::Stretch,
                };
            }
            "aspect-ratio" => {
                style.aspect_ratio = parse_aspect_ratio(value);
            }
            "overflow" | "overflow-x" | "overflow-y" => {
                style.overflow_clip = value.split_whitespace().any(|token| {
                    matches!(
                        token.to_ascii_lowercase().as_str(),
                        "hidden" | "clip" | "scroll" | "auto" | "overlay"
                    )
                });
            }
            "grid-column" | "grid-row" => {
                if let Some(placement) = parse_grid_placement(value) {
                    if name.as_str() == "grid-column" {
                        style.grid_column = placement;
                    } else {
                        style.grid_row = placement;
                    }
                }
            }
            "grid-column-start" => {
                if let Some(line) = parse_grid_line(value) {
                    style.grid_column.start = line;
                }
            }
            "grid-column-end" => {
                if let Some(line) = parse_grid_line(value) {
                    style.grid_column.end = line;
                }
            }
            "grid-row-start" => {
                if let Some(line) = parse_grid_line(value) {
                    style.grid_row.start = line;
                }
            }
            "grid-row-end" => {
                if let Some(line) = parse_grid_line(value) {
                    style.grid_row.end = line;
                }
            }
            "gap" => {
                if let Some(gap) = parse_gap_em(value, style.font_size) {
                    style.gap = gap;
                }
            }
            "row-gap" => {
                if let Some(gap) = parse_dimension_em(value, style.font_size) {
                    style.gap[0] = gap;
                }
            }
            "column-gap" => {
                if let Some(gap) = parse_dimension_em(value, style.font_size) {
                    style.gap[1] = gap;
                }
            }
            "flex-grow" => {
                if let Ok(grow) = value.parse::<f32>() {
                    style.flex_grow = grow.max(0.0);
                }
            }
            "flex-shrink" => {
                if let Ok(shrink) = value.parse::<f32>() {
                    style.flex_shrink = shrink.max(0.0);
                }
            }
            "flex-basis" => style.flex_basis = parse_dimension_em(value, style.font_size),
            "color" => {
                if let Some(color) = parse_color(value) {
                    style.color = color;
                }
            }
            "background" | "background-color" => style.background = parse_background_color(value),
            // Ya aplicado en la pasada previa, con el tamaño final del bloque.
            "font-size" => {}
            "width" => style.width = parse_dimension_em(value, style.font_size),
            "height" => style.height = parse_dimension_em(value, style.font_size),
            "font-weight" => style.font_weight = parse_font_weight(value, style.font_weight),
            "text-align" => {
                style.text_align = match value.to_ascii_lowercase().as_str() {
                    "center" => TextAlignKind::Center,
                    "right" | "end" => TextAlignKind::Right,
                    _ => TextAlignKind::Left,
                };
            }
            "text-transform" => {
                style.text_transform = match value.to_ascii_lowercase().as_str() {
                    "uppercase" => TextTransformKind::Uppercase,
                    "lowercase" => TextTransformKind::Lowercase,
                    "capitalize" => TextTransformKind::Capitalize,
                    _ => TextTransformKind::None,
                };
            }
            "text-decoration" | "text-decoration-line" => {
                style.underline = value
                    .split_whitespace()
                    .any(|token| token.eq_ignore_ascii_case("underline"));
                style.line_through = value
                    .split_whitespace()
                    .any(|token| token.eq_ignore_ascii_case("line-through"));
            }
            "position" => {
                style.position = match value.to_ascii_lowercase().as_str() {
                    "relative" => PositionKind::Relative,
                    "absolute" => PositionKind::Absolute,
                    _ => PositionKind::Static,
                };
            }
            "top" => style.insets[0] = parse_dimension_em(value, style.font_size),
            "right" => style.insets[1] = parse_dimension_em(value, style.font_size),
            "bottom" => style.insets[2] = parse_dimension_em(value, style.font_size),
            "left" => style.insets[3] = parse_dimension_em(value, style.font_size),
            "line-height" => style.line_height = parse_line_height(value, style.font_size),
            "min-width" => style.min_width = parse_dimension_em(value, style.font_size),
            "max-width" => style.max_width = parse_dimension_em(value, style.font_size),
            "min-height" => style.min_height = parse_dimension_em(value, style.font_size),
            "max-height" => style.max_height = parse_dimension_em(value, style.font_size),
            "list-style-type" => {
                style.list_style = parse_list_style(value).unwrap_or(style.list_style)
            }
            "list-style" => {
                // Shorthand: el primer token reconocible manda (`square inside`).
                if let Some(kind) = split_css_components(value)
                    .into_iter()
                    .find_map(parse_list_style)
                {
                    style.list_style = kind;
                }
            }
            "visibility" => {
                style.visibility =
                    if matches!(value.to_ascii_lowercase().as_str(), "hidden" | "collapse") {
                        VisibilityKind::Hidden
                    } else {
                        VisibilityKind::Visible
                    };
            }
            "overflow-wrap" | "word-wrap" => {
                style.overflow_wrap = matches!(
                    value.to_ascii_lowercase().as_str(),
                    "break-word" | "anywhere"
                );
            }
            "white-space" => {
                style.white_space = match value.to_ascii_lowercase().as_str() {
                    "nowrap" => WhiteSpaceKind::NoWrap,
                    "pre" => WhiteSpaceKind::Pre,
                    "pre-wrap" | "break-spaces" => WhiteSpaceKind::PreWrap,
                    _ => WhiteSpaceKind::Normal,
                };
            }
            "box-sizing" => {
                style.box_sizing = if value.eq_ignore_ascii_case("border-box") {
                    BoxSizingKind::BorderBox
                } else {
                    BoxSizingKind::ContentBox
                };
            }
            "margin" => {
                if let Some((values, autos)) = parse_margin_values(value, style.font_size) {
                    style.margin = values;
                    style.margin_auto = autos;
                }
            }
            "padding" => {
                if let Some(values) = parse_box_values_em(value, style.font_size) {
                    style.padding = values;
                }
            }
            "margin-top" => set_margin_side(&mut style, 0, value),
            "margin-right" => set_margin_side(&mut style, 1, value),
            "margin-bottom" => set_margin_side(&mut style, 2, value),
            "margin-left" => set_margin_side(&mut style, 3, value),
            "padding-top" => set_box_side(&mut style.padding, 0, value, style.font_size),
            "padding-right" => set_box_side(&mut style.padding, 1, value, style.font_size),
            "padding-bottom" => set_box_side(&mut style.padding, 2, value, style.font_size),
            "padding-left" => set_box_side(&mut style.padding, 3, value, style.font_size),
            "border" => parse_border_shorthand(value, &mut style),
            "border-width" => {
                if let Some(values) = parse_box_values_em(value, style.font_size) {
                    style.border_width = values;
                }
            }
            "border-color" => set_border_colors(&mut style, value),
            "border-style" => set_border_styles(&mut style, value),
            "border-radius" => {
                if let Some(radius) = parse_dimension_em(value, style.font_size) {
                    style.border_radius = radius;
                }
            }
            "border-top" => parse_border_side(value, &mut style, 0),
            "border-right" => parse_border_side(value, &mut style, 1),
            "border-bottom" => parse_border_side(value, &mut style, 2),
            "border-left" => parse_border_side(value, &mut style, 3),
            "border-top-width" => set_box_side(&mut style.border_width, 0, value, style.font_size),
            "border-right-width" => {
                set_box_side(&mut style.border_width, 1, value, style.font_size)
            }
            "border-bottom-width" => {
                set_box_side(&mut style.border_width, 2, value, style.font_size)
            }
            "border-left-width" => set_box_side(&mut style.border_width, 3, value, style.font_size),
            "border-top-color" => set_border_color(&mut style, 0, value),
            "border-right-color" => set_border_color(&mut style, 1, value),
            "border-bottom-color" => set_border_color(&mut style, 2, value),
            "border-left-color" => set_border_color(&mut style, 3, value),
            "border-top-style" => set_border_style(&mut style, 0, value),
            "border-right-style" => set_border_style(&mut style, 1, value),
            "border-bottom-style" => set_border_style(&mut style, 2, value),
            "border-left-style" => set_border_style(&mut style, 3, value),
            _ => {}
        }
    }
    style
}

/// Divide un valor CSS por espacios de primer nivel, sin cortar dentro de
/// paréntesis: `calc(1rem + 4px)` es un solo componente.
fn split_css_components(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (index, character) in value.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth -= 1,
            character if character.is_whitespace() && depth == 0 => {
                let part = value[start..index].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    let part = value[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

fn parse_box_values_em(value: &str, font_size: f32) -> Option<[Length; 4]> {
    let values = split_css_components(value)
        .into_iter()
        .map(|token| parse_dimension_em(token, font_size))
        .collect::<Option<Vec<_>>>()?;
    expand_box_values(values)
}

/// Aplica la expansión 1–4 valores de los shorthands de caja de CSS
/// (arriba, derecha, abajo, izquierda).
fn expand_box_values<T: Copy>(values: Vec<T>) -> Option<[T; 4]> {
    match values.as_slice() {
        [all] => Some([*all; 4]),
        [vertical, horizontal] => Some([*vertical, *horizontal, *vertical, *horizontal]),
        [top, horizontal, bottom] => Some([*top, *horizontal, *bottom, *horizontal]),
        [top, right, bottom, left] => Some([*top, *right, *bottom, *left]),
        _ => None,
    }
}

/// `margin`: acepta `auto` en cualquier lado además de longitudes.
fn parse_margin_values(value: &str, font_size: f32) -> Option<([Length; 4], [bool; 4])> {
    let mut lengths = Vec::new();
    let mut autos = Vec::new();
    for token in split_css_components(value) {
        if token.eq_ignore_ascii_case("auto") {
            lengths.push(Length::Px(0.0));
            autos.push(true);
        } else {
            lengths.push(parse_dimension_em(token, font_size)?);
            autos.push(false);
        }
    }
    Some((expand_box_values(lengths)?, expand_box_values(autos)?))
}

fn set_margin_side(style: &mut Style, side: usize, value: &str) {
    if value.trim().eq_ignore_ascii_case("auto") {
        style.margin[side] = Length::Px(0.0);
        style.margin_auto[side] = true;
        return;
    }
    if let Some(length) = parse_dimension_em(value, style.font_size) {
        style.margin[side] = length;
        style.margin_auto[side] = false;
    }
}

fn set_box_side(sides: &mut [Length; 4], index: usize, value: &str, font_size: f32) {
    if let Some(length) = parse_dimension_em(value, font_size) {
        sides[index] = length;
    }
}

fn parse_length(value: &str) -> Option<f32> {
    let value = value.trim().strip_suffix("px").unwrap_or(value.trim());
    let length = value.parse::<f32>().ok()?;
    (length.is_finite() && length >= 0.0).then_some(length)
}

/// Una longitud CSS absoluta: `px`, `em`, `rem`, `pt` o `pc`. `em` se
/// resuelve con el `font-size` del elemento y `rem` con el de la raíz (16 px).
fn parse_absolute_length(value: &str, font_size: f32) -> Option<f32> {
    let (number, scale) = absolute_length_parts(value, font_size)?;
    let length = number * scale;
    (length.is_finite() && length >= 0.0).then_some(length)
}

/// Igual que [`parse_absolute_length`], pero admite valores negativos, como
/// los que puede producir una resta dentro de `calc()`.
fn parse_signed_length(value: &str, font_size: f32) -> Option<f32> {
    let (number, scale) = absolute_length_parts(value, font_size)?;
    let length = number * scale;
    length.is_finite().then_some(length)
}

fn absolute_length_parts(value: &str, font_size: f32) -> Option<(f32, f32)> {
    let value = value.trim();
    let (number, scale) = if let Some(number) = value.strip_suffix("px") {
        (number, 1.0)
    } else if let Some(number) = value.strip_suffix("rem") {
        (number, ROOT_FONT_SIZE)
    } else if let Some(number) = value.strip_suffix("em") {
        (number, font_size.max(1.0))
    } else if let Some(number) = value.strip_suffix("pt") {
        (number, 96.0 / 72.0)
    } else if let Some(number) = value.strip_suffix("pc") {
        (number, 16.0)
    } else {
        (value, 1.0)
    };
    Some((number.trim().parse::<f32>().ok()?, scale))
}

/// Longitud de caja (`width`, `margin`, `gap`, `flex-basis`…): `px`, `em`,
/// `rem`, `pt`, `pc`, `%` o una expresión `calc()` de longitudes.
fn parse_dimension_em(value: &str, font_size: f32) -> Option<Length> {
    let value = value.trim();
    if let Some((kind, args)) = strip_length_function(value) {
        return parse_length_function(kind, args, font_size);
    }
    if let Some(expression) = strip_calc(value) {
        return parse_calc_expression(expression, font_size);
    }
    // Un paréntesis que envuelve toda la expresión también puede aparecer
    // como operando de `calc()`, por ejemplo `calc((100% - 40px) / 2)`.
    if let Some(expression) = strip_parentheses(value) {
        return parse_calc_expression(expression, font_size);
    }
    if let Some(percent) = value.strip_suffix('%') {
        let percent = percent.parse::<f32>().ok()?;
        return (percent.is_finite() && percent >= 0.0).then_some(Length::Percent(percent));
    }
    parse_absolute_length(value, font_size).map(Length::Px)
}

/// `min(...)`, `max(...)` o `clamp(...)`: devuelve el tipo de función y sus
/// argumentos sin los paréntesis exteriores.
fn strip_length_function(value: &str) -> Option<(LengthFunction, &str)> {
    let value = value.trim();
    let inner = value.strip_suffix(')')?;
    let open = inner.find('(')?;
    let name = inner[..open].trim();
    let kind = if name.eq_ignore_ascii_case("min") {
        LengthFunction::Min
    } else if name.eq_ignore_ascii_case("max") {
        LengthFunction::Max
    } else if name.eq_ignore_ascii_case("clamp") {
        LengthFunction::Clamp
    } else {
        return None;
    };
    Some((kind, &inner[open + 1..]))
}

/// `min(a, b)`, `max(a, b, c)` y `clamp(min, val, max)` con argumentos que
/// pueden ser longitudes, porcentajes, `calc()` o funciones anidadas.
fn parse_length_function(kind: LengthFunction, args: &str, font_size: f32) -> Option<Length> {
    let parts = split_top_level_commas(args);
    let count = match kind {
        LengthFunction::Clamp if parts.len() == 3 => 3,
        LengthFunction::Min | LengthFunction::Max if parts.len() == 2 => 2,
        LengthFunction::Min | LengthFunction::Max if parts.len() == 3 => 3,
        _ => return None,
    };
    let mut values = [[0.0f32; 2]; 3];
    for (index, part) in parts.iter().take(count).enumerate() {
        let length = parse_dimension_em(part.trim(), font_size)?;
        // Anidar una función que dependa del contenedor no es representable
        // con coeficientes lineales; se descarta la declaración, como haría
        // un navegador con un valor no soportado.
        if matches!(length, Length::Function { .. }) && !length.is_absolute() {
            return None;
        }
        let (px, percent) = length_components(length);
        values[index] = [px, percent];
    }
    Some(Length::Function {
        kind,
        count: count as u8,
        args: values,
    })
}

fn strip_calc(value: &str) -> Option<&str> {
    let value = value.trim();
    let prefix = value.get(..5)?;
    if !prefix.eq_ignore_ascii_case("calc(") || !value.ends_with(')') {
        return None;
    }
    Some(value[5..value.len() - 1].trim())
}

/// Quita un paréntesis que envuelve toda la expresión.
fn strip_parentheses(value: &str) -> Option<&str> {
    let value = value.trim();
    if !value.starts_with('(') || !value.ends_with(')') {
        return None;
    }
    let inner = &value[1..value.len() - 1];
    let mut depth = 0i32;
    for character in inner.chars() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    (depth == 0).then_some(inner.trim())
}

fn length_components(length: Length) -> (f32, f32) {
    match length {
        Length::Px(px) => (px, 0.0),
        Length::Percent(percent) => (0.0, percent),
        Length::Sum { px, percent } => (px, percent),
        // Una función dependiente del contenedor no se puede plegar a una
        // combinación lineal; el parser sólo la admite dentro de `calc()`
        // cuando es absoluta, así que aquí siempre es plegable.
        Length::Function { .. } => (length.resolve(0.0), 0.0),
    }
}

/// Resuelve una expresión de `calc()` sobre longitudes. Cualquier suma de
/// longitudes de CSS es una combinación lineal `px + % del contenedor`, que
/// es exactamente lo que Wisp puede resolver al maquetar.
fn parse_calc_expression(expression: &str, font_size: f32) -> Option<Length> {
    let mut px = 0.0f32;
    let mut percent = 0.0f32;
    for (sign, term) in split_calc_terms(expression)? {
        let (term_px, term_percent) = length_components(parse_calc_term(term, font_size)?);
        px += sign * term_px;
        percent += sign * term_percent;
    }
    if !px.is_finite() || !percent.is_finite() {
        return None;
    }
    Some(if percent == 0.0 {
        Length::Px(px)
    } else if px == 0.0 {
        Length::Percent(percent)
    } else {
        Length::Sum { px, percent }
    })
}

/// Separa los sumandos de una expresión de `calc()`, con su signo.
fn split_calc_terms(expression: &str) -> Option<Vec<(f32, &str)>> {
    let bytes = expression.as_bytes();
    let mut terms = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut sign = 1.0f32;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'+' | b'-' if depth == 0 && index > 0 => {
                let term = expression[start..index].trim();
                if term.is_empty() {
                    return None;
                }
                terms.push((sign, term));
                sign = if bytes[index] == b'-' { -1.0 } else { 1.0 };
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    let term = expression[start..].trim();
    if term.is_empty() {
        return None;
    }
    terms.push((sign, term));
    Some(terms)
}

fn parse_calc_term(term: &str, font_size: f32) -> Option<Length> {
    let term = term.trim();
    if let Some(inner) = strip_calc(term) {
        return parse_calc_expression(inner, font_size);
    }
    if let Some(inner) = strip_parentheses(term) {
        return parse_calc_expression(inner, font_size);
    }
    if let Some((left, right)) = term.split_once('*') {
        let (left, right) = (left.trim(), right.trim());
        if let Ok(scale) = left.parse::<f32>() {
            return scale_length(parse_dimension_em(right, font_size)?, scale);
        }
        if let Ok(scale) = right.parse::<f32>() {
            return scale_length(parse_dimension_em(left, font_size)?, scale);
        }
        return None;
    }
    if let Some((left, right)) = term.split_once('/') {
        let divisor = right.trim().parse::<f32>().ok()?;
        if divisor == 0.0 {
            return None;
        }
        return scale_length(parse_dimension_em(left.trim(), font_size)?, 1.0 / divisor);
    }
    if let Some(percent) = term.strip_suffix('%') {
        let percent = percent.trim().parse::<f32>().ok()?;
        return percent.is_finite().then_some(Length::Percent(percent));
    }
    parse_signed_length(term, font_size).map(Length::Px)
}

fn scale_length(length: Length, scale: f32) -> Option<Length> {
    if !scale.is_finite() {
        return None;
    }
    let (px, percent) = length_components(length);
    Some(if percent == 0.0 {
        Length::Px(px * scale)
    } else if px == 0.0 {
        Length::Percent(percent * scale)
    } else {
        Length::Sum {
            px: px * scale,
            percent: percent * scale,
        }
    })
}

/// `font-size`: acepta longitudes absolutas y `%`/`em` relativos al tamaño
/// heredado (el de la raíz es siempre 16 px para `rem`).
fn parse_font_size(value: &str, inherited: f32) -> Option<f32> {
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        let percent = percent.parse::<f32>().ok()?;
        return Some(inherited * percent / 100.0);
    }
    parse_absolute_length(value, inherited)
}

/// `line-height`: número sin unidades (multiplicador del `font-size`),
/// porcentaje, `normal` o longitud absoluta.
fn parse_line_height(value: &str, font_size: f32) -> Option<f32> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("normal") {
        return None;
    }
    if let Some(percent) = value.strip_suffix('%') {
        let percent = percent.parse::<f32>().ok()?;
        return (percent.is_finite() && percent > 0.0)
            .then(|| (font_size * percent / 100.0).clamp(1.0, 512.0));
    }
    if let Ok(number) = value.parse::<f32>() {
        return (number.is_finite() && number > 0.0)
            .then(|| (font_size * number).clamp(1.0, 512.0));
    }
    parse_absolute_length(value, font_size).map(|height| height.clamp(1.0, 512.0))
}

fn parse_font_weight(value: &str, inherited: u16) -> u16 {
    match value.to_ascii_lowercase().as_str() {
        "normal" => 400,
        "bold" | "bolder" => 700,
        "lighter" => 300,
        _ => value
            .parse::<u16>()
            .ok()
            .map(|weight| weight.clamp(100, 900) / 100 * 100)
            .unwrap_or(inherited),
    }
}

/// `background` puede incluir imagen, posición o repetición: se usa el primer
/// token que sea un color válido.
fn parse_background_color(value: &str) -> Option<Color> {
    parse_color(value).or_else(|| {
        split_css_components(value)
            .into_iter()
            .find_map(parse_color)
    })
}

fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim().to_ascii_lowercase();
    // `rgb()`/`rgba()` aceptan comas o espacios y alfa tras `/`.
    if let Some(inner) = value
        .strip_prefix("rgb(")
        .or_else(|| value.strip_prefix("rgba("))
        .and_then(|inner| inner.strip_suffix(')'))
    {
        let normalized = inner.replace('/', " ");
        let parts: Vec<&str> = normalized
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() != 3 && parts.len() != 4 {
            return None;
        }
        let channel = |part: &str| -> Option<u8> {
            let part = part.trim();
            let value = match part.strip_suffix('%') {
                Some(percent) => percent.trim().parse::<f32>().ok()? * 2.55,
                None => part.parse::<f32>().ok()?,
            };
            Some(value.round().clamp(0.0, 255.0) as u8)
        };
        let alpha = match parts.get(3) {
            None => 255,
            Some(alpha) => {
                let alpha = alpha.trim();
                let value = match alpha.strip_suffix('%') {
                    Some(percent) => percent.trim().parse::<f32>().ok()? / 100.0,
                    None => alpha.parse::<f32>().ok()?,
                };
                (value.clamp(0.0, 1.0) * 255.0).round() as u8
            }
        };
        return Some(Color {
            r: channel(parts[0])?,
            g: channel(parts[1])?,
            b: channel(parts[2])?,
            a: alpha,
        });
    }
    match value.as_str() {
        "black" => Some(Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        }),
        "white" => Some(Color {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        }),
        "red" => Some(Color {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        }),
        "green" => Some(Color {
            r: 0,
            g: 128,
            b: 0,
            a: 255,
        }),
        "blue" => Some(Color {
            r: 0,
            g: 0,
            b: 255,
            a: 255,
        }),
        "gray" | "grey" => Some(Color {
            r: 128,
            g: 128,
            b: 128,
            a: 255,
        }),
        _ => {
            let hex = value.strip_prefix('#')?;
            if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            if hex.len() == 3 {
                let mut channels = hex
                    .chars()
                    .map(|digit| digit.to_digit(16).map(|v| (v * 17) as u8));
                Some(Color {
                    r: channels.next()??,
                    g: channels.next()??,
                    b: channels.next()??,
                    a: 255,
                })
            } else if hex.len() == 6 {
                Some(Color {
                    r: u8::from_str_radix(&hex[0..2], 16).ok()?,
                    g: u8::from_str_radix(&hex[2..4], 16).ok()?,
                    b: u8::from_str_radix(&hex[4..6], 16).ok()?,
                    a: 255,
                })
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn text_commands(document: &LayoutDocument) -> Vec<&str> {
        document
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::Text { content, .. } => Some(content.as_str()),
                PaintCommand::FillRect { .. }
                | PaintCommand::Image { .. }
                | PaintCommand::StrokeRect { .. }
                | PaintCommand::BackgroundImage { .. }
                | PaintCommand::Clip { .. }
                | PaintCommand::ClipEnd => None,
            })
            .collect()
    }

    #[test]
    fn overflow_hidden_emits_a_clip_region() {
        let document = layout_html(
            r#"<div style="width:200px;height:100px;overflow:hidden"><div style="height:400px"></div></div>"#,
            640.0,
        )
        .unwrap();
        let clips = document
            .commands
            .iter()
            .filter(|command| matches!(command, PaintCommand::Clip { .. }))
            .count();
        let ends = document
            .commands
            .iter()
            .filter(|command| matches!(command, PaintCommand::ClipEnd))
            .count();
        assert_eq!((clips, ends), (1, 1));
        let Some(PaintCommand::Clip { rect }) = document
            .commands
            .iter()
            .find(|command| matches!(command, PaintCommand::Clip { .. }))
        else {
            panic!("expected a clip command");
        };
        assert_eq!((rect.width, rect.height), (200.0, 100.0));
    }

    #[test]
    fn css_min_max_and_clamp_resolve_against_the_container() {
        let document = layout_html(
            r#"<div style="width:min(100%,400px);height:10px;background:#010101"></div><div style="width:clamp(100px,50%,300px);height:10px;background:#020202"></div><div style="width:max(200px,10%);height:10px;background:#030303"></div>"#,
            640.0,
        )
        .unwrap();
        let widths: Vec<f32> = document
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::FillRect { rect, .. } if rect.height == 10.0 => Some(rect.width),
                _ => None,
            })
            .collect();
        assert_eq!(widths, vec![400.0, 300.0, 200.0]);
    }

    #[test]
    fn html5_parser_recovers_malformed_markup_and_creates_paint_commands() {
        let document = layout_html("<h1>One<h1>Two", 800.0).unwrap();
        let text = text_commands(&document);
        assert!(text.contains(&"One"));
        assert!(text.contains(&"Two"));
    }

    #[test]
    fn inline_styles_set_background_text_color_and_padding() {
        let document = layout_html(
            r#"<div style="background-color:#123456;color:red;padding:4px">Wisp</div>"#,
            200.0,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::FillRect { color, rect, .. }
                if *color == Color { r: 18, g: 52, b: 86, a: 255 }
                    && rect.height >= 8.0
        )));
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { color, content, .. }
                if *color == Color { r: 255, g: 0, b: 0, a: 255 }
                    && content == "Wisp"
        )));
    }

    #[test]
    fn linear_gradient_background_emits_background_image_command() {
        let document = layout_html(
            r#"<div style="background:linear-gradient(to right, red, blue)">Wisp</div>"#,
            300.0,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::BackgroundImage { image: BgImage::LinearGradient { angle_deg, stops }, .. }
                if (*angle_deg - 90.0).abs() < 0.01
                    && stops.len() == 2
                    && stops[0].offset == 0.0
                    && stops[1].offset == 1.0
        )));
    }

    #[test]
    fn background_image_url_resolves_against_page_url() {
        let document = layout_html_with_stylesheets_and_url(
            r#"<div style="background-image:url('hero.png')">Wisp</div>"#,
            &[],
            Some("https://example.com/docs/page.html"),
            300.0,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::BackgroundImage { image: BgImage::Url(url), .. }
                if url == "https://example.com/docs/hero.png"
        )));
    }

    #[test]
    fn background_image_from_stylesheet_rule_is_painted() {
        let document = layout_html_with_stylesheets(
            r#"<style>.hero{background-image:linear-gradient(45deg,#fff,#000)}</style><div class="hero">Wisp</div>"#,
            &[],
            300.0,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::BackgroundImage { image: BgImage::LinearGradient { angle_deg, .. }, .. }
                if (*angle_deg - 45.0).abs() < 0.01
        )));
    }

    #[test]
    fn inline_span_can_override_inherited_text_color() {
        let document = layout_html(
            r#"<p style="color:blue">plain <span style="color:red">red</span></p>"#,
            300.0,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { color, content, .. }
                if *color == Color { r: 0, g: 0, b: 255, a: 255 }
                    && content == "plain"
        )));
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { color, content, .. }
                if *color == Color { r: 255, g: 0, b: 0, a: 255 }
                    && content == "red"
        )));
    }

    #[test]
    fn link_targets_survive_layout_for_navigation() {
        let document = layout_html(r#"<p><a href="/next">Open page</a></p>"#, 300.0).unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { href: Some(href), content, .. }
                if href == "/next" && content == "Open"
        )));
    }

    #[test]
    fn external_css_applies_class_and_id_rules_with_cascade_order() {
        let html = r#"<main><p id="notice" class="message">Visible</p><p class="hidden">Hidden</p></main>"#;
        let stylesheets = vec![
            "p { color: #ff0000; } .message { color: #00ff00; } #notice { color: #123456; } .hidden { display: none; }"
                .to_string(),
        ];
        let document = layout_html_with_stylesheets(html, &stylesheets, 300.0).unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { content, color, .. }
                if content == "Visible" && *color == Color { r: 18, g: 52, b: 86, a: 255 }
        )));
        assert!(!text_commands(&document).contains(&"Hidden"));
    }

    #[test]
    fn inline_javascript_can_update_text_through_the_dom() {
        let document = layout_html(
            r##"<p id="message">Before</p><script>document.querySelector("#message").textContent = "Wisp works";</script>"##,
            300.0,
        )
        .unwrap();
        let text = text_commands(&document);
        assert!(text.contains(&"Wisp"));
        assert!(text.contains(&"works"));
        assert!(!text.contains(&"Before"));
        assert!(document.warnings.is_empty());
    }

    #[test]
    fn css_applies_to_elements_created_by_javascript() {
        let html = r#"<main id="root"></main><script>
            const card = document.createElement("div");
            card.className = "card";
            card.textContent = "Created";
            document.getElementById("root").appendChild(card);
        </script>"#;
        let stylesheets = vec![".card { color: #123456; }".to_string()];
        let document = layout_html_with_stylesheets(html, &stylesheets, 300.0).unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { content, color, .. }
                if content == "Created" && *color == Color { r: 18, g: 52, b: 86, a: 255 }
        )));
    }

    #[test]
    fn click_handlers_change_what_is_painted() {
        let document = layout_html(
            r##"<button id="button">Open</button><p id="out">before</p>
            <script>
                document.getElementById("button").addEventListener("click", () => {
                    document.getElementById("out").textContent = "after";
                });
                document.getElementById("button").click();
            </script>"##,
            300.0,
        )
        .unwrap();
        let text = text_commands(&document);
        assert!(text.contains(&"after"));
        assert!(!text.contains(&"before"));
    }

    #[test]
    fn page_url_is_exposed_to_javascript() {
        let document = layout_html_with_stylesheets_and_url(
            r##"<p id="out"></p><script>
                document.getElementById("out").textContent = location.pathname;
            </script>"##,
            &[],
            Some("https://example.com/docs/page?q=1"),
            300.0,
        )
        .unwrap();
        assert!(text_commands(&document).contains(&"/docs/page"));
    }

    #[test]
    fn inline_javascript_loop_limit_keeps_the_page_renderable() {
        let document = layout_html(
            "<script>while (true) {}</script><p>Still visible</p>",
            300.0,
        )
        .unwrap();
        assert!(text_commands(&document).contains(&"Still"));
        assert!(
            document
                .warnings
                .iter()
                .any(|warning| warning.contains("Falló el script inline"))
        );
    }

    #[test]
    fn external_javascript_without_a_source_warns_without_blocking_static_rendering() {
        let document = layout_html(
            r#"<script src="/app.js"></script><p>Static fallback</p>"#,
            300.0,
        )
        .unwrap();
        assert!(text_commands(&document).contains(&"Static"));
        assert!(
            document
                .warnings
                .iter()
                .any(|warning| warning.contains("script externo"))
        );
    }

    #[test]
    fn hidden_content_is_not_painted_and_text_wraps() {
        let document = layout_html(
            r#"<div style="display:none">secret</div><p>one two three four five</p>"#,
            45.0,
        )
        .unwrap();
        let text = text_commands(&document);
        assert!(!text.iter().any(|line| line.contains("secret")));
        assert!(text.len() > 1);
        assert!(document.content_height > 0.0);
    }

    #[test]
    fn rejects_invalid_viewport_width() {
        assert!(layout_html("<p>hi</p>", f32::NAN).is_err());
        assert!(layout_html("<p>hi</p>", 0.0).is_err());
    }

    #[test]
    fn wisp_network_loader_rejects_non_web_schemes_and_url_credentials() {
        assert!(fetch_url("file:///C:/private.html").is_err());
        assert!(fetch_url("https://user:password@example.com/").is_err());
    }

    #[test]
    fn page_scripts_can_fetch_and_xhr_data_from_the_same_origin() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            let mut served = 0usize;
            while served < 3 && std::time::Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(_) => {
                        thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                };
                served += 1;
                stream.set_nonblocking(false).unwrap();
                let mut request = [0; 2048];
                let read = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]).to_string();
                let (content_type, body) = if request.starts_with("GET /data.json ") {
                    ("application/json", r#"{"message":"desde fetch"}"#)
                } else if request.starts_with("GET /xhr.json ") {
                    ("application/json", r#"{"message":"desde xhr"}"#)
                } else {
                    (
                        "text/html; charset=utf-8",
                        r#"<html><body><p id="a">inicio</p><p id="b">inicio</p><script>
fetch("/data.json").then(function (response) { return response.json(); }).then(function (data) {
  document.getElementById("a").textContent = data.message;
});
var xhr = new XMLHttpRequest();
xhr.open("GET", "/xhr.json");
xhr.onload = function () {
  document.getElementById("b").textContent = JSON.parse(xhr.responseText).message;
};
xhr.send();
</script></body></html>"#,
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let page = fetch_url(&format!("http://{address}/")).unwrap();
        let layout = page.layout(800.0).unwrap();
        server.join().unwrap();
        let texts = text_commands(&layout).join(" ");
        assert!(
            texts.contains("desde fetch"),
            "textos: {texts} / avisos: {:?}",
            layout.warnings
        );
        assert!(
            texts.contains("desde xhr"),
            "textos: {texts} / avisos: {:?}",
            layout.warnings
        );
    }

    #[test]
    fn cross_origin_fetch_blocked_by_cors_reports_the_reason() {
        let page_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let page_address = page_listener.local_addr().unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let data_address = data_listener.local_addr().unwrap();
        page_listener.set_nonblocking(true).unwrap();
        data_listener.set_nonblocking(true).unwrap();

        // El servidor de datos responde SIN Access-Control-Allow-Origin.
        let data_server = thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            while std::time::Instant::now() < deadline {
                let (mut stream, _) = match data_listener.accept() {
                    Ok(connection) => connection,
                    Err(_) => {
                        thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let mut request = [0; 1024];
                let _ = stream.read(&mut request);
                let body = r#"{"ok":1}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
                break;
            }
        });

        let html = format!(
            r#"<p id="a">inicio</p><script>
fetch("http://{data_address}/data.json").then(function () {{
  document.getElementById("a").textContent = "sin bloqueo";
}}).catch(function (error) {{
  document.getElementById("a").textContent = error.message;
}});
</script>"#
        );
        let page_server = thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            while std::time::Instant::now() < deadline {
                let (mut stream, _) = match page_listener.accept() {
                    Ok(connection) => connection,
                    Err(_) => {
                        thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let mut request = [0; 2048];
                let _ = stream.read(&mut request);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    html.len(),
                    html
                )
                .unwrap();
                break;
            }
        });

        let page = fetch_url(&format!("http://{page_address}/")).unwrap();
        let layout = page.layout(800.0).unwrap();
        page_server.join().unwrap();
        data_server.join().unwrap();
        let texts = text_commands(&layout).join(" ");
        assert!(
            texts.contains("CORS bloqueó"),
            "textos: {texts} / avisos: {:?}",
            layout.warnings
        );
        assert!(
            layout
                .warnings
                .iter()
                .any(|warning| warning.contains("fetch(): CORS")),
            "avisos: {:?}",
            layout.warnings
        );
    }

    #[test]
    fn wisp_network_loader_fetches_html_and_reads_its_title() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let (content_type, body) = if request.starts_with("GET /style.css ") {
                    ("text/css", ".message { color: #123456; }")
                } else {
                    (
                        "text/html; charset=utf-8",
                        "<html><head><title>Local Wisp</title><link rel=\"stylesheet\" href=\"/style.css\"></head><body><p class=\"message\">hello</p></body></html>",
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let page = fetch_url(&format!("http://{address}/")).unwrap();
        server.join().unwrap();
        assert_eq!(page.final_url, format!("http://{address}/"));
        assert_eq!(page.title, "Local Wisp");
        assert!(page.html.contains("<p class=\"message\">hello</p>"));
        assert_eq!(page.stylesheets, [".message { color: #123456; }"]);
        assert!(page.warnings.is_empty());
    }

    #[test]
    fn wisp_network_loader_fetches_and_executes_external_scripts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let (content_type, body) = if request.starts_with("GET /app.js ") {
                    (
                        "application/javascript",
                        r#"document.getElementById("out").textContent = "loaded";"#,
                    )
                } else {
                    (
                        "text/html; charset=utf-8",
                        r#"<html><head><title>External</title></head><body><p id="out">static</p><script src="/app.js"></script></body></html>"#,
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let page = fetch_url(&format!("http://{address}/")).unwrap();
        server.join().unwrap();
        assert_eq!(page.external_scripts.len(), 1);
        let document = page.layout(300.0).unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { content, .. } if content == "loaded"
        )));
    }

    #[test]
    fn module_specifier_scanner_finds_static_and_dynamic_imports() {
        let source = r#"
            import "./side-effect.js";
            import { a, b as c } from "./named.js";
            import * as ns from "/absolute.js";
            import "https://cdn.example.com/lib.js";
            export { d } from "./reexport.js";
            export * from './star.js';
            const lazy = import("./lazy.js");
            const dynamic = import(`./template.js`);
            const text = "import './not-a-real-import.js'";
            // import "./commented.js";
            /* import "./block-comment.js"; */
            import.meta.url;
        "#;
        let mut specifiers = module_specifiers(source);
        specifiers.sort();
        assert_eq!(
            specifiers,
            [
                "./lazy.js",
                "./named.js",
                "./reexport.js",
                "./side-effect.js",
                "./star.js",
                "./template.js",
                "/absolute.js",
                "https://cdn.example.com/lib.js",
            ]
        );
    }

    #[test]
    fn module_specifier_scanner_ignores_identifiers_inside_strings_and_comments() {
        let source = r#"
            const a = "export x from './nope.js'";
            // import x from "./also-nope.js";
            const b = `import y from './still-nope.js'`;
            import ok from "./ok.js";
        "#;
        assert_eq!(module_specifiers(source), ["./ok.js"]);
    }

    #[test]
    fn module_url_resolution_rejects_bare_specifiers_and_non_web_urls() {
        assert_eq!(
            resolve_module_url("https://example.com/app/main.js", "./util.js").as_deref(),
            Some("https://example.com/app/util.js")
        );
        assert_eq!(
            resolve_module_url("https://example.com/app/main.js", "../root.js").as_deref(),
            Some("https://example.com/root.js")
        );
        assert!(resolve_module_url("https://example.com/", "react").is_none());
        assert!(resolve_module_url("https://example.com/", "data:text/javascript,1").is_none());
        assert!(resolve_module_url("https://example.com/", "file:///etc/passwd").is_none());
    }

    #[test]
    fn wisp_network_loader_fetches_and_executes_es_modules() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let (content_type, body) = if request.starts_with("GET /main.js ") {
                    (
                        "application/javascript",
                        r#"import { value } from "./lib/util.js";
                           document.getElementById("out").textContent = value;"#,
                    )
                } else if request.starts_with("GET /lib/util.js ") {
                    (
                        "application/javascript",
                        r#"export const value = "module";"#,
                    )
                } else {
                    (
                        "text/html; charset=utf-8",
                        r#"<html><head><title>Modules</title></head><body><p id="out">static</p><script type="module" src="/main.js"></script></body></html>"#,
                    )
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });

        let page = fetch_url(&format!("http://{address}/")).unwrap();
        server.join().unwrap();
        assert_eq!(page.module_scripts.len(), 2);
        assert!(
            page.module_scripts
                .contains_key(&format!("http://{address}/lib/util.js"))
        );
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        let document = page.layout(300.0).unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { content, .. } if content == "module"
        )));
    }

    #[test]
    fn parses_short_hex_colors() {
        assert_eq!(
            parse_color("#0af"),
            Some(Color {
                r: 0,
                g: 170,
                b: 255,
                a: 255,
            })
        );
        assert_eq!(parse_color("#éabcd"), None);
    }

    /// 1x1 PNG whose IHDR is patched to 100x50; `imagesize` reads the header
    /// without validating CRC, which is all Wisp needs for layout.
    const TINY_PNG_100X50: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x00, 0x32, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn images_are_fetched_and_painted_inline_with_text() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let png = TINY_PNG_100X50.to_vec();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 1024];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                if request.starts_with("GET /img.png ") {
                    let mut head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        png.len()
                    )
                    .into_bytes();
                    head.extend_from_slice(&png);
                    stream.write_all(&head).unwrap();
                } else {
                    let body = r#"<html><head><title>Img</title></head><body><p>before <img src="/img.png" width="50" alt="pic"> after</p></body></html>"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            }
        });

        let page = fetch_url(&format!("http://{address}/")).unwrap();
        server.join().unwrap();
        let image_url = format!("http://{address}/img.png");
        let resource = page
            .images
            .get(&image_url)
            .expect("image should be fetched");
        assert_eq!((resource.width, resource.height), (100, 50));
        let document = page.layout(300.0).unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Image { rect, url, .. }
                if url == &image_url && rect.width == 50.0 && rect.height == 25.0
        )));
        let text = text_commands(&document);
        assert!(text.contains(&"before"));
        assert!(text.contains(&"after"));
    }

    #[test]
    fn data_uri_images_layout_without_fetching() {
        let data_uri = "data:image/png;base64,aGk=";
        let mut images = HashMap::new();
        images.insert(
            data_uri.to_owned(),
            ImageResource {
                data: b"hi".to_vec(),
                width: 1,
                height: 1,
            },
        );
        let document = layout_html_with_all_resources(
            &format!(r#"<p>left <img src="{data_uri}" width="20"> right</p>"#),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            Some("https://example.com/"),
            300.0,
            &images,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Image { url, rect, .. }
                if url == data_uri && rect.width == 20.0 && rect.height == 20.0
        )));
    }

    #[test]
    fn missing_images_fall_back_to_alt_text() {
        let document = layout_html_with_all_resources(
            r#"<p>before <img src="/gone.png" alt="broken picture"> after</p>"#,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            Some("https://example.com/"),
            300.0,
            &HashMap::new(),
        )
        .unwrap();
        assert!(
            !document
                .commands
                .iter()
                .any(|command| matches!(command, PaintCommand::Image { .. }))
        );
        let text = text_commands(&document);
        assert!(text.contains(&"broken"));
        assert!(text.contains(&"picture"));
    }

    #[test]
    fn images_inside_links_keep_their_href() {
        let data_uri = "data:image/png;base64,aGk=";
        let mut images = HashMap::new();
        images.insert(
            data_uri.to_owned(),
            ImageResource {
                data: b"hi".to_vec(),
                width: 4,
                height: 4,
            },
        );
        let document = layout_html_with_all_resources(
            &format!(r#"<p><a href="/full.png"><img src="{data_uri}"></a></p>"#),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            Some("https://example.com/"),
            300.0,
            &images,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Image { url, href: Some(href), .. }
                if url == data_uri && href == "/full.png"
        )));
    }

    #[test]
    fn display_inline_keeps_block_tags_in_the_same_flow_line() {
        let document = layout_html(
            r#"<div style="display:inline">first</div><div style="display:inline">second</div>"#,
            300.0,
        )
        .unwrap();
        let positions = document
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::Text { content, y, .. } => Some((content.as_str(), *y)),
                PaintCommand::FillRect { .. }
                | PaintCommand::Image { .. }
                | PaintCommand::StrokeRect { .. }
                | PaintCommand::BackgroundImage { .. }
                | PaintCommand::Clip { .. }
                | PaintCommand::ClipEnd => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].1, positions[1].1);
    }

    fn text_positions(document: &LayoutDocument) -> Vec<(String, f32, f32)> {
        document
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::Text { content, x, y, .. } => Some((content.clone(), *x, *y)),
                PaintCommand::FillRect { .. }
                | PaintCommand::Image { .. }
                | PaintCommand::StrokeRect { .. }
                | PaintCommand::BackgroundImage { .. }
                | PaintCommand::Clip { .. }
                | PaintCommand::ClipEnd => None,
            })
            .collect()
    }

    #[test]
    fn flex_row_places_items_side_by_side() {
        let document = layout_html(
            r#"<div style="display:flex"><div style="width:50px">izquierda</div><div style="width:50px">derecha</div></div>"#,
            400.0,
        )
        .unwrap();
        let positions = text_positions(&document);
        let left = positions.iter().find(|(t, _, _)| t == "izquierda").unwrap();
        let right = positions.iter().find(|(t, _, _)| t == "derecha").unwrap();
        assert!((left.2 - right.2).abs() < 1.0, "misma línea: {positions:?}");
        assert!(right.1 > left.1 + 40.0, "a la derecha: {positions:?}");
    }

    #[test]
    fn flex_column_stacks_items_vertically() {
        let document = layout_html(
            r#"<div style="display:flex;flex-direction:column"><div>uno</div><div>dos</div></div>"#,
            400.0,
        )
        .unwrap();
        let positions = text_positions(&document);
        let one = positions.iter().find(|(t, _, _)| t == "uno").unwrap();
        let two = positions.iter().find(|(t, _, _)| t == "dos").unwrap();
        assert!(two.2 > one.2, "debajo: {positions:?}");
    }

    #[test]
    fn flex_gap_and_center_position_items() {
        let document = layout_html(
            r#"<div style="display:flex;gap:20px;justify-content:center"><div style="width:50px">a</div><div style="width:50px">b</div></div>"#,
            400.0,
        )
        .unwrap();
        let positions = text_positions(&document);
        let a = positions.iter().find(|(t, _, _)| t == "a").unwrap();
        let b = positions.iter().find(|(t, _, _)| t == "b").unwrap();
        // Contenedor empieza en x=0 (body tiene margen pero el div flex es hijo
        // de body); los items miden 50px con gap 20 -> offset centrado = (400-8-120)/2 + 8 ≈ 144.
        assert!(a.1 > 100.0, "centrado: {positions:?}");
        assert!(
            (b.1 - a.1 - 50.0 - 20.0).abs() < 30.0,
            "gap ~20: {positions:?}"
        );
    }

    #[test]
    fn grid_two_columns_places_items_in_cells() {
        let document = layout_html(
            r#"<div style="display:grid;grid-template-columns:100px 1fr"><div>celda1</div><div>celda2</div><div>celda3</div></div>"#,
            400.0,
        )
        .unwrap();
        let positions = text_positions(&document);
        let c1 = positions.iter().find(|(t, _, _)| t == "celda1").unwrap();
        let c2 = positions.iter().find(|(t, _, _)| t == "celda2").unwrap();
        let c3 = positions.iter().find(|(t, _, _)| t == "celda3").unwrap();
        assert!(c2.1 > c1.1 + 50.0, "segunda columna: {positions:?}");
        assert!(c3.2 > c1.2, "segunda fila: {positions:?}");
        assert!((c3.1 - c1.1).abs() < 5.0, "misma columna: {positions:?}");
    }

    #[test]
    fn flex_wrap_moves_items_to_next_line() {
        let document = layout_html(
            r#"<div style="display:flex;flex-wrap:wrap;width:120px"><div style="width:100px">uno</div><div style="width:100px">dos</div></div>"#,
            400.0,
        )
        .unwrap();
        let positions = text_positions(&document);
        let one = positions.iter().find(|(t, _, _)| t == "uno").unwrap();
        let two = positions.iter().find(|(t, _, _)| t == "dos").unwrap();
        assert!(two.2 > one.2, "wrap a otra línea: {positions:?}");
    }

    #[test]
    fn headings_and_bold_elements_carry_font_weight() {
        let document = layout_html("<h1>Titulo</h1><p><b>negrita</b> normal</p>", 400.0).unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { content, font_weight, .. }
                if content == "Titulo" && *font_weight == 700
        )));
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { content, font_weight, .. }
                if content == "negrita" && *font_weight == 700
        )));
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { content, font_weight, .. }
                if content == "normal" && *font_weight == 400
        )));
    }

    #[test]
    fn text_align_center_shifts_the_line_right() {
        let left = layout_html("<div><p>centrado</p></div>", 400.0).unwrap();
        let center = layout_html(
            "<div style=\"text-align:center\"><p>centrado</p></div>",
            400.0,
        )
        .unwrap();
        let x_of = |document: &LayoutDocument| {
            text_positions(document)
                .into_iter()
                .find(|(text, _, _)| text == "centrado")
                .map(|(_, x, _)| x)
                .unwrap()
        };
        assert!(x_of(&center) > x_of(&left) + 50.0);
    }

    #[test]
    fn percentage_width_resolves_against_the_container() {
        let document = layout_html(
            r#"<div style="width:50%;background:#123456">mitad</div>"#,
            400.0,
        )
        .unwrap();
        // body tiene margen 8 -> contenido 384; 50% = 192.
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::FillRect { rect, .. } if (rect.width - 192.0).abs() < 2.0
        )));
    }

    #[test]
    fn box_sizing_border_box_includes_padding_in_the_width() {
        let border = layout_html(
            r#"<div style="width:100px;padding:10px;box-sizing:border-box;background:#123456">x</div>"#,
            400.0,
        )
        .unwrap();
        let content = layout_html(
            r#"<div style="width:100px;padding:10px;background:#123456">x</div>"#,
            400.0,
        )
        .unwrap();
        let has_width = |document: &LayoutDocument, expected: f32| {
            document.commands.iter().any(|command| {
                matches!(
                    command,
                    PaintCommand::FillRect { rect, .. } if (rect.width - expected).abs() < 2.0
                )
            })
        };
        assert!(has_width(&border, 100.0), "{:?}", border.commands);
        assert!(has_width(&content, 120.0), "{:?}", content.commands);
    }

    #[test]
    fn border_shorthand_paints_a_stroke_and_offsets_content() {
        let document = layout_html(
            r#"<div style="border:4px solid #ff0000;padding:10px"><p>dentro</p></div>"#,
            400.0,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::StrokeRect { color, width, .. }
                if *color == Color { r: 255, g: 0, b: 0, a: 255 } && *width == 4.0
        )));
        // El texto empieza tras border(4) + padding(10) = 14px desde el borde.
        let text_x = text_positions(&document)
            .into_iter()
            .find(|(t, _, _)| t == "dentro")
            .map(|(_, x, _)| x)
            .unwrap();
        assert!(text_x > 20.0, "x={text_x}");
    }

    #[test]
    fn border_radius_reaches_the_background_rect() {
        let document = layout_html(
            r#"<div style="width:100px;height:50px;background:#123456;border-radius:10px"></div>"#,
            400.0,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::FillRect { radius, .. } if (*radius - 10.0).abs() < 0.5
        )));
    }

    #[test]
    fn lists_paint_bullets_and_ordered_numbers() {
        let document = layout_html(
            "<ul><li>manzana</li><li>pera</li></ul><ol><li>uno</li><li>dos</li></ol>",
            400.0,
        )
        .unwrap();
        let texts: Vec<&str> = text_commands(&document);
        assert!(texts.contains(&"•"), "{texts:?}");
        assert!(texts.contains(&"1."), "{texts:?}");
        assert!(texts.contains(&"2."), "{texts:?}");
        assert!(texts.contains(&"manzana"));
        // Las listas tienen sangría: el contenido no empieza en x=8.
        let x = text_positions(&document)
            .into_iter()
            .find(|(t, _, _)| t == "manzana")
            .map(|(_, x, _)| x)
            .unwrap();
        assert!(x > 40.0, "x={x}");
    }

    fn fetched_page(html: &str) -> FetchedPage {
        FetchedPage::new(
            "https://example.com/article".to_owned(),
            "t".to_owned(),
            html.to_owned(),
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            Vec::new(),
        )
    }

    #[test]
    fn reader_text_extracts_visible_text_with_block_breaks() {
        let page = fetched_page(
            "<html><body><h1>Título</h1><p>Hola <b>mundo</b></p>\
             <div style=\"display:none\">secreto</div><script>var x=1;</script></body></html>",
        );
        let text = page.reader_text().unwrap();
        assert!(text.contains("Título"), "{text}");
        assert!(text.contains("Hola mundo"), "{text}");
        assert!(!text.contains("secreto"), "{text}");
        assert!(!text.contains("var x"), "{text}");
        assert!(text.contains('\n'), "{text}");
    }

    #[test]
    fn reader_text_returns_none_for_empty_pages() {
        assert!(
            fetched_page("<html><body></body></html>")
                .reader_text()
                .is_none()
        );
    }

    #[test]
    fn reader_text_truncates_on_char_boundary_with_multibyte_text() {
        // >200 KiB de texto con caracteres de 2 bytes: el tope de
        // MAX_READER_CHARS puede caer dentro de una «ñ» y truncate() paniquea
        // si no se retrocede a la frontera UTF-8.
        let text = "ñ".repeat(120_000);
        let html = format!("<html><body><p>{text}</p></body></html>");
        let page = fetched_page(&html);
        let extracted = page.reader_text().unwrap();
        assert!(extracted.len() <= MAX_READER_CHARS);
        assert!(extracted.chars().all(|c| c == 'ñ' || c == ' ' || c == '\n'));
    }

    #[test]
    fn url_suits_wisp_matches_documentation_and_articles() {
        for url in [
            "https://es.wikipedia.org/wiki/Rust",
            "https://docs.rs/serde/latest/serde/",
            "https://developer.mozilla.org/es/docs/Web",
            "https://example.com/blog/mi-post",
            "https://example.com/docs/guide",
            "https://blog.ejemplo.com/2024/01/articulo",
        ] {
            assert!(url_suits_wisp(url), "{url}");
        }
        for url in [
            "https://mail.google.com/mail/u/0/",
            "https://github.com/rust-lang/rust",
            "https://www.youtube.com/watch?v=x",
            "https://app.mi-banco.com/cuenta",
            "https://example.com/p/12345",
            "https://example.com/news/ultima-hora",
            "not a url",
        ] {
            assert!(!url_suits_wisp(url), "{url}");
        }
    }

    fn text_color(document: &LayoutDocument, word: &str) -> Color {
        document
            .commands
            .iter()
            .find_map(|command| match command {
                PaintCommand::Text { content, color, .. } if content == word => Some(*color),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no se pintó {word:?}: {:?}", document.commands))
    }

    #[test]
    fn media_queries_follow_the_viewport_width() {
        let html = "<p>responsive</p>";
        let stylesheets = vec![
            "p { color: #000000; } \
             @media (min-width: 700px) { p { color: #ff0000; } } \
             @media screen and (max-width: 699px) { p { color: #0000ff; } }"
                .to_string(),
        ];
        let narrow = layout_html_with_stylesheets(html, &stylesheets, 400.0).unwrap();
        assert_eq!(
            text_color(&narrow, "responsive"),
            Color {
                r: 0,
                g: 0,
                b: 255,
                a: 255
            }
        );
        let wide = layout_html_with_stylesheets(html, &stylesheets, 1000.0).unwrap();
        assert_eq!(
            text_color(&wide, "responsive"),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
    }

    #[test]
    fn media_queries_are_re_evaluated_when_a_fetched_page_resizes() {
        let page = FetchedPage::new(
            "https://example.com/".to_owned(),
            "t".to_owned(),
            "<p>texto</p>".to_owned(),
            vec!["@media (max-width: 500px) { p { color: #ff0000; } }".to_owned()],
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            Vec::new(),
        );
        let wide = page.layout(900.0).unwrap();
        assert_eq!(
            text_color(&wide, "texto"),
            Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255
            }
        );
        let narrow = page.layout(420.0).unwrap();
        assert_eq!(
            text_color(&narrow, "texto"),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        // Volver al ancho anterior reutiliza el documento preparado.
        let again = page.layout(900.0).unwrap();
        assert_eq!(text_color(&again, "texto"), text_color(&wide, "texto"));
    }

    #[test]
    fn css_custom_properties_inherit_and_support_fallbacks() {
        let html = r#"<div class="card"><p class="title">Hola</p><p class="muted">Gris</p></div>"#;
        let stylesheets = vec![
            ":root { --brand: #123456; --missing: var(--nope, #00ff00); } \
             .card { --brand: #ff0000; } \
             .title { color: var(--brand); } \
             .muted { color: var(--missing); background: var(--unknown, #0000ff); }"
                .to_string(),
        ];
        let document = layout_html_with_stylesheets(html, &stylesheets, 400.0).unwrap();
        assert_eq!(
            text_color(&document, "Hola"),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );
        assert_eq!(
            text_color(&document, "Gris"),
            Color {
                r: 0,
                g: 255,
                b: 0,
                a: 255
            }
        );
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::FillRect { color, .. }
                if *color == Color { r: 0, g: 0, b: 255, a: 255 }
        )));
    }

    #[test]
    fn em_rem_and_percentage_font_sizes_resolve_against_their_parents() {
        let document = layout_html(
            r#"<div style="font-size:2em;padding:1rem"><span style="font-size:150%">texto</span></div>"#,
            400.0,
        )
        .unwrap();
        let (_, x, _) = text_positions(&document)
            .into_iter()
            .find(|(word, _, _)| word == "texto")
            .expect("texto pintado");
        // body margin 8 + padding 1rem (16) = 24.
        assert!((x - 24.0).abs() < 0.5, "x={x}");
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::Text { content, font_size, .. }
                if content == "texto" && (*font_size - 48.0).abs() < 0.1
        )));
    }

    #[test]
    fn text_transform_and_underline_are_painted() {
        let document = layout_html(
            r#"<a href="/x">enlace</a><div style="text-transform:uppercase">minusculas</div>"#,
            400.0,
        )
        .unwrap();
        let text = text_commands(&document);
        assert!(text.contains(&"enlace"), "{text:?}");
        assert!(text.contains(&"MINUSCULAS"), "{text:?}");
        // Subrayado del enlace: una regla de 1 px bajo las palabras.
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::FillRect { rect, .. } if rect.height == 1.0 && rect.width > 10.0
        )));
    }

    #[test]
    fn line_height_adds_space_and_centers_text_in_the_line_box() {
        let tight = layout_html(
            "<div><p style=\"margin:0\">uno</p><p style=\"margin:0\">dos</p></div>",
            400.0,
        )
        .unwrap();
        let loose = layout_html(
            "<div style=\"line-height:3\"><p style=\"margin:0\">uno</p><p style=\"margin:0\">dos</p></div>",
            400.0,
        )
        .unwrap();
        let y_of = |document: &LayoutDocument, word: &str| {
            text_positions(document)
                .into_iter()
                .find(|(content, _, _)| content == word)
                .map(|(_, _, y)| y)
                .unwrap()
        };
        let tight_dos = y_of(&tight, "dos");
        let loose_dos = y_of(&loose, "dos");
        assert!(loose_dos > tight_dos + 20.0, "{tight_dos} -> {loose_dos}");
        // El texto se centra en una caja de 48 px, no queda pegado arriba.
        assert!(y_of(&loose, "uno") > 12.0);
    }

    #[test]
    fn br_and_pre_preserve_line_breaks_and_spaces() {
        let document = layout_html("<p>uno<br>dos</p><pre>linea1\n  linea2</pre>", 400.0).unwrap();
        let positions = text_positions(&document);
        let y_of = |word: &str| {
            positions
                .iter()
                .find(|(content, _, _)| content == word)
                .map(|(_, _, y)| *y)
                .unwrap()
        };
        assert!(y_of("dos") > y_of("uno"), "{positions:?}");
        let text = text_commands(&document);
        assert!(text.contains(&"linea1"), "{text:?}");
        assert!(text.contains(&"  linea2"), "{text:?}");
        assert!(y_of("  linea2") > y_of("linea1"), "{positions:?}");
    }

    #[test]
    fn css_specificity_uses_ids_classes_and_element_counts() {
        let document = layout_html_with_stylesheets(
            r#"<p class="note" id="lead">Hola</p>"#,
            &["#lead { color: #ff0000; } .note { color: #0000ff; } \
                 p.note { color: #00ff00; } p { color: #000000; }"
                .to_string()],
            400.0,
        )
        .unwrap();
        assert_eq!(
            text_color(&document, "Hola"),
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255
            }
        );

        let class_plus_element = layout_html_with_stylesheets(
            r#"<p class="note">Hola</p>"#,
            &[".note { color: #0000ff; } p.note { color: #00ff00; }".to_string()],
            400.0,
        )
        .unwrap();
        assert_eq!(
            text_color(&class_plus_element, "Hola"),
            Color {
                r: 0,
                g: 255,
                b: 0,
                a: 255
            }
        );
    }

    #[test]
    fn border_longhands_and_dashed_styles_paint_per_side() {
        let document = layout_html(
            r#"<div style="border-top:2px solid #ff0000;border-left:2px dashed #0000ff;height:20px"></div>"#,
            200.0,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::FillRect { rect, color, .. }
                if *color == Color { r: 255, g: 0, b: 0, a: 255 }
                    && rect.height == 2.0
                    && rect.width > 150.0
        )));
        let blue_stripes = document
            .commands
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    PaintCommand::FillRect { rect, color, .. }
                        if *color == Color { r: 0, g: 0, b: 255, a: 255 }
                            && rect.width == 2.0
                            && rect.height < 10.0
                )
            })
            .count();
        assert!(blue_stripes > 1, "guiones: {blue_stripes}");
    }

    #[test]
    fn horizontal_rule_paints_a_one_pixel_line() {
        let document = layout_html("<p>a</p><hr><p>b</p>", 300.0).unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::FillRect { rect, color, .. }
                if rect.height == 1.0 && *color == Color { r: 200, g: 200, b: 200, a: 255 }
        )));
    }

    fn fill_widths(document: &LayoutDocument) -> Vec<f32> {
        document
            .commands
            .iter()
            .filter_map(|command| match command {
                PaintCommand::FillRect { rect, .. } => Some(rect.width),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn min_and_max_width_clamp_the_box() {
        let minimum = layout_html(
            r#"<div style="width:40px;min-width:120px;height:10px;background:#123456"></div>"#,
            400.0,
        )
        .unwrap();
        assert!(
            fill_widths(&minimum)
                .iter()
                .any(|width| (*width - 120.0).abs() < 1.0),
            "{:?}",
            fill_widths(&minimum)
        );

        let maximum = layout_html(
            r#"<div style="width:500px;max-width:150px;height:10px;background:#123456"></div>"#,
            400.0,
        )
        .unwrap();
        assert!(
            fill_widths(&maximum)
                .iter()
                .any(|width| (*width - 150.0).abs() < 1.0),
            "{:?}",
            fill_widths(&maximum)
        );
    }

    #[test]
    fn min_height_expands_short_boxes() {
        let document = layout_html(
            r#"<div style="height:10px;min-height:60px;background:#123456"></div>"#,
            400.0,
        )
        .unwrap();
        assert!(document.commands.iter().any(|command| matches!(
            command,
            PaintCommand::FillRect { rect, .. } if (rect.height - 60.0).abs() < 1.0
        )));
    }

    #[test]
    fn calc_resolves_percentages_pixels_and_rem() {
        let document = layout_html(
            r#"<div style="width:calc(100% - 40px);height:10px;background:#123456"></div>"#,
            400.0,
        )
        .unwrap();
        // body: 400 - 16 de margen = 384; 384 - 40 = 344.
        assert!(
            fill_widths(&document)
                .iter()
                .any(|width| (*width - 344.0).abs() < 1.0),
            "{:?}",
            fill_widths(&document)
        );

        let half = layout_html(
            r#"<div style="width:calc((100% - 40px) / 2);height:10px;background:#123456"></div>"#,
            400.0,
        )
        .unwrap();
        assert!(
            fill_widths(&half)
                .iter()
                .any(|width| (*width - 172.0).abs() < 1.0),
            "{:?}",
            fill_widths(&half)
        );

        let padded = layout_html(
            r#"<div style="padding:calc(1rem + 4px);background:#123456"><p style="margin:0">x</p></div>"#,
            400.0,
        )
        .unwrap();
        let (_, x, _) = text_positions(&padded)
            .into_iter()
            .find(|(word, _, _)| word == "x")
            .expect("texto pintado");
        // margen del body (8) + padding 1rem + 4px (20).
        assert!((x - 28.0).abs() < 0.5, "x={x}");
    }

    #[test]
    fn list_style_controls_the_marker() {
        let without = layout_html(
            r#"<ul style="list-style:none"><li>sin marca</li></ul>"#,
            300.0,
        )
        .unwrap();
        assert!(!text_commands(&without).contains(&"•"));

        let square = layout_html(
            r#"<ul style="list-style-type:square"><li>cuadro</li></ul>"#,
            300.0,
        )
        .unwrap();
        assert!(text_commands(&square).contains(&"▪"));

        let alpha = layout_html(
            r#"<ol style="list-style-type:lower-alpha"><li>uno</li><li>dos</li></ol>"#,
            300.0,
        )
        .unwrap();
        let text = text_commands(&alpha);
        assert!(text.contains(&"a.") && text.contains(&"b."), "{text:?}");

        let roman = layout_html(
            r#"<ol style="list-style-type:upper-roman"><li>uno</li></ol>"#,
            300.0,
        )
        .unwrap();
        assert!(text_commands(&roman).contains(&"I."));
    }

    #[test]
    fn visibility_hidden_keeps_space_without_painting() {
        let document = layout_html(
            r#"<p>uno</p><div style="visibility:hidden">oculto</div><p>dos</p>"#,
            300.0,
        )
        .unwrap();
        let text = text_commands(&document);
        assert!(!text.contains(&"oculto"), "{text:?}");
        assert!(text.contains(&"uno") && text.contains(&"dos"), "{text:?}");
        let positions = text_positions(&document);
        let y_of = |word: &str| {
            positions
                .iter()
                .find(|(content, _, _)| content == word)
                .map(|(_, _, y)| *y)
                .unwrap()
        };
        // El bloque oculto conserva su altura: el párrafo siguiente no sube.
        assert!(y_of("dos") > y_of("uno") + 20.0, "{positions:?}");
    }

    #[test]
    fn overflow_wrap_breaks_a_long_word_across_lines() {
        let word = "supercalifragilisticoespialidoso";
        let document = layout_html(
            &format!(r#"<p style="overflow-wrap:break-word;width:80px;margin:0">{word}</p>"#),
            300.0,
        )
        .unwrap();
        let chunks = text_commands(&document);
        assert!(chunks.len() > 1, "{chunks:?}");
        for chunk in &chunks {
            assert!(
                text_width(chunk, 16.0) <= 81.0,
                "trozo demasiado ancho: {chunk:?}"
            );
        }
    }

    #[test]
    fn auto_margins_center_a_fixed_width_box() {
        let document = layout_html(
            r#"<div style="width:100px;margin:0 auto;height:10px;background:#123456"></div>"#,
            400.0,
        )
        .unwrap();
        let rect = document
            .commands
            .iter()
            .find_map(|command| match command {
                PaintCommand::FillRect { rect, color, .. }
                    if *color
                        == Color {
                            r: 18,
                            g: 52,
                            b: 86,
                            a: 255,
                        } =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .expect("fondo pintado");
        assert!((rect.width - 100.0).abs() < 1.0, "{rect:?}");
        // body (margen 8) → 384 disponibles; (384 - 100) / 2 + 8 = 150.
        assert!((rect.x - 150.0).abs() < 1.0, "{rect:?}");
    }

    #[test]
    fn important_declarations_beat_specificity() {
        let document = layout_html_with_stylesheets(
            r#"<div id="target">hola</div>"#,
            &[
                "#target { color: rgb(255, 0, 0); } div { color: rgb(0, 128, 0) !important; }"
                    .to_owned(),
            ],
            400.0,
        )
        .unwrap();
        let color = document
            .commands
            .iter()
            .find_map(|command| match command {
                PaintCommand::Text { content, color, .. } if content.contains("hola") => {
                    Some(*color)
                }
                _ => None,
            })
            .expect("texto pintado");
        assert_eq!(
            color,
            Color {
                r: 0,
                g: 128,
                b: 0,
                a: 255
            }
        );
    }

    #[test]
    fn relative_position_shifts_painting_but_not_the_flow() {
        let document = layout_html(
            r#"<div style="position:relative;top:20px;left:30px;height:10px;background:#010203"></div>"#,
            400.0,
        )
        .unwrap();
        let rect = document
            .commands
            .iter()
            .find_map(|command| match command {
                PaintCommand::FillRect { rect, color, .. }
                    if *color
                        == Color {
                            r: 1,
                            g: 2,
                            b: 3,
                            a: 255,
                        } =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .expect("fondo pintado");
        assert!((rect.x - 38.0).abs() < 1.0, "{rect:?}");
        assert!((rect.y - 28.0).abs() < 1.0, "{rect:?}");
    }

    #[test]
    fn absolute_children_leave_the_flow_and_honor_insets() {
        let document = layout_html(
            r#"<div style="position:relative">
                <div style="position:absolute;right:0;top:0;width:20px;height:20px;background:#0a0b0c"></div>
                <p style="margin:0">flujo</p>
            </div>"#,
            400.0,
        )
        .unwrap();
        let rect = document
            .commands
            .iter()
            .find_map(|command| match command {
                PaintCommand::FillRect { rect, color, .. }
                    if *color
                        == Color {
                            r: 10,
                            g: 11,
                            b: 12,
                            a: 255,
                        } =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .expect("fondo pintado");
        // body 8 + contenido 384; el bloque absoluto se pega al borde derecho.
        assert!((rect.x - 372.0).abs() < 1.0, "{rect:?}");
        assert!((rect.y - 8.0).abs() < 1.0, "{rect:?}");
        // El párrafo sigue empezando arriba: el absoluto no ocupa espacio.
        let (_, _, y) = text_positions(&document)
            .into_iter()
            .find(|(word, _, _)| word == "flujo")
            .expect("texto");
        assert!((y - 8.0).abs() < 2.0, "{y}");
    }

    #[test]
    fn fixed_position_is_treated_as_static() {
        let static_document = layout_html(r#"<div>texto</div>"#, 400.0).unwrap();
        let fixed_document =
            layout_html(r#"<div style="position:fixed;top:80px">texto</div>"#, 400.0).unwrap();
        assert_eq!(
            text_positions(&static_document),
            text_positions(&fixed_document)
        );
    }

    #[test]
    fn line_through_paint_a_rule_over_the_text() {
        let document = layout_html(
            r#"<span style="text-decoration:line-through">tachado</span>"#,
            400.0,
        )
        .unwrap();
        let (_, _, text_y) = text_positions(&document)
            .into_iter()
            .find(|(word, _, _)| word == "tachado")
            .expect("texto");
        let rule_y = document
            .commands
            .iter()
            .find_map(|command| match command {
                PaintCommand::FillRect { rect, .. } if rect.height == 1.0 => Some(rect.y),
                _ => None,
            })
            .expect("regla");
        assert!(
            rule_y > text_y && rule_y < text_y + 9.0,
            "{rule_y} {text_y}"
        );
    }

    #[test]
    fn media_query_ranges_compare_the_viewport_width() {
        let html = r#"<p>x</p>"#;
        let css = vec![
            "@media (width >= 600px) { p { color: rgb(0, 0, 255); } }
             @media (600px < width) { p { font-weight: 700; } }"
                .to_owned(),
        ];
        let wide = layout_html_with_stylesheets(html, &css, 800.0).unwrap();
        let color_of = |document: &LayoutDocument| {
            document.commands.iter().find_map(|command| match command {
                PaintCommand::Text { content, color, .. } if content.contains('x') => Some(*color),
                _ => None,
            })
        };
        assert_eq!(
            color_of(&wide),
            Some(Color {
                r: 0,
                g: 0,
                b: 255,
                a: 255
            })
        );
        let narrow = layout_html_with_stylesheets(html, &css, 500.0).unwrap();
        assert_ne!(
            color_of(&narrow),
            Some(Color {
                r: 0,
                g: 0,
                b: 255,
                a: 255
            })
        );
    }

    #[test]
    fn stylesheet_imports_are_found_in_every_css_syntax() {
        let css = r#"@import url("a.css");
            @import 'b.css' screen;
            @IMPORT url( c.css ) layer(base);
            @import url("d.css") supports(display:grid);"#;
        assert_eq!(
            stylesheet_import_urls(css),
            vec!["a.css", "b.css", "c.css", "d.css"]
        );
    }

    #[test]
    fn rgb_and_rgba_colors_parse_comma_and_space_syntax() {
        assert_eq!(
            parse_color("rgb(255, 0, 17)"),
            Some(Color {
                r: 255,
                g: 0,
                b: 17,
                a: 255
            })
        );
        assert_eq!(
            parse_color("rgba(0 128 255 / 50%)"),
            Some(Color {
                r: 0,
                g: 128,
                b: 255,
                a: 128
            })
        );
        assert_eq!(
            parse_color("rgb(50%, 100%, 0%)"),
            Some(Color {
                r: 128,
                g: 255,
                b: 0,
                a: 255
            })
        );
    }
}
