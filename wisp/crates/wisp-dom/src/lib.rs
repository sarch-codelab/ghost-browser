//! Modelo `Page -> Navigable -> Document` de Wisp (backlog, ítem 8).
//!
//! Es la capa de navegación del motor: una [`Page`] tiene un [`Navigable`]
//! (URL actual + historial) y un [`Document`] (origen, título y HTML vivo).
//! Los iframes son documentos hijos con origen opaco, como manda la spec.
//!
//! Este crate es el hogar futuro del DOM que hoy vive como `DomNode` en
//! `wisp-engine` (`wisp/src/lib.rs`). No depende de `scraper`/`ego-tree`:
//! describe el modelo, no el parser. Cuando `wisp-engine` migre, `FetchedPage`
//! pasará a envolver una `Page` en vez de guardar `html` suelto.
//!
//! *Hecho cuando* (ítem 8): pasan los tests WPT de
//! `html/semantics/embedded-content` y `html/webappapis/dynamic-markup-insertion`.
//! Este crate cubre la parte de modelo (origen opaco, `document.write()`,
//! historial); el parser que los alimenta sigue en `wisp-engine`.

#![forbid(unsafe_code)]

use std::collections::HashMap;

/// Origen de un documento. `Opaque` es el de los iframes `srcdoc`/sandbox y
/// de `about:blank` heredado: nunca es `same_origin` con nadie, ni consigo
/// mismo (dos orígenes opacos distintos no comparten nada).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Origin {
    Opaque,
    Http {
        scheme: String,
        host: String,
        port: u16,
    },
}

impl Origin {
    /// Deriva el origen de una URL http(s). Cualquier otra URL (o una URL
    /// inválida) produce un origen opaco, que es el valor seguro por defecto.
    pub fn from_url(url: &str) -> Self {
        let Some((scheme, rest)) = url.split_once("://") else {
            return Origin::Opaque;
        };
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Origin::Opaque;
        }
        // `host[:port][/...]`: el host termina en `/`, `?`, `#` o `:`.
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
                (host, port.parse::<u16>().unwrap_or(if scheme == "https" { 443 } else { 80 }))
            }
            _ => (authority, if scheme == "https" { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Origin::Opaque;
        }
        Origin::Http {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        }
    }

    /// `true` si ambos orígenes son el mismo origen http(s). Un origen opaco
    /// nunca es `same_origin`, ni siquiera con otro opaco.
    pub fn same_origin(&self, other: &Origin) -> bool {
        self == other && matches!(self, Origin::Http { .. })
    }
}

/// Un documento: URL, origen, título y HTML vivo.
///
/// `html` es el cuerpo tal cual llegó de la red; `written` acumula lo que
/// `document.write()` añadió (se serializa tras `html`, como en un navegador
/// que inserta en el stream de parsing). Cuando el motor migre su parser,
/// `document_html()` será el árbol serializado, no una concatenación.
#[derive(Debug, Clone)]
pub struct Document {
    url: String,
    origin: Origin,
    title: String,
    html: String,
    written: String,
}

impl Document {
    pub fn new(url: &str, html: &str) -> Self {
        Self {
            url: url.to_owned(),
            origin: Origin::from_url(url),
            title: title_from_html(html),
            html: html.to_owned(),
            written: String::new(),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// `document.write(chunk)`: añade al stream del documento. En un navegador
    /// real esto puede re-parsear; aquí se acumula para serializar después.
    pub fn write(&mut self, chunk: &str) {
        self.written.push_str(chunk);
    }

    /// HTML vivo: cuerpo original + lo escrito por `document.write()`.
    pub fn document_html(&self) -> String {
        if self.written.is_empty() {
            self.html.clone()
        } else {
            format!("{}{}", self.html, self.written)
        }
    }
}

/// Extrae `<title>...</title>` (primera aparición, sin entidades).
fn title_from_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<title>") else {
        return String::new();
    };
    let rest = &html[start + "<title>".len()..];
    let Some(end) = rest.to_ascii_lowercase().find("</title>") else {
        return String::new();
    };
    rest[..end].trim().to_owned()
}

/// Navegación: URL actual + historial con índice (atrás/adelante).
///
/// Navegar corta el futuro (lo que había delante del índice se descarta),
/// igual que en un navegador.
#[derive(Debug, Clone)]
pub struct Navigable {
    url: String,
    history: Vec<String>,
    index: usize,
}

impl Navigable {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_owned(),
            history: vec![url.to_owned()],
            index: 0,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn index(&self) -> usize {
        self.index
    }

    /// Navega a `url` como entrada nueva de historial.
    pub fn navigate(&mut self, url: &str) {
        self.history.truncate(self.index + 1);
        self.history.push(url.to_owned());
        self.index = self.history.len() - 1;
        self.url = url.to_owned();
    }

    /// Paso atrás. Devuelve la URL nueva, o `None` si no hay dónde ir.
    pub fn back(&mut self) -> Option<&str> {
        if self.index == 0 {
            return None;
        }
        self.index -= 1;
        self.url = self.history[self.index].clone();
        Some(&self.url)
    }

    /// Paso adelante. Devuelve la URL nueva, o `None` si no hay dónde ir.
    pub fn forward(&mut self) -> Option<&str> {
        if self.index + 1 >= self.history.len() {
            return None;
        }
        self.index += 1;
        self.url = self.history[self.index].clone();
        Some(&self.url)
    }
}

/// Una página: un navegable, su documento y los iframes hijos.
///
/// Los iframes son [`Document`] con origen opaco (salvo que su `src` sea
/// http(s) del mismo origen, caso en el que heredan ese origen). Se guardan
/// por nombre para que el motor pueda resolverlos por `id`/`name`.
#[derive(Debug, Clone)]
pub struct Page {
    navigable: Navigable,
    document: Document,
    iframes: HashMap<String, Document>,
}

impl Page {
    pub fn load(url: &str, html: &str) -> Self {
        Self {
            navigable: Navigable::new(url),
            document: Document::new(url, html),
            iframes: HashMap::new(),
        }
    }

    pub fn navigable(&self) -> &Navigable {
        &self.navigable
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.document
    }

    /// Navega la página a `url` con `html` ya descargado: actualiza navegable
    /// y documento a la vez, como hace `wisp-agent serve` hoy por separado.
    pub fn navigate(&mut self, url: &str, html: &str) {
        self.navigable.navigate(url);
        self.document = Document::new(url, html);
        self.iframes.clear();
    }

    /// Añade un iframe hijo. `src` vacío/`about:blank` produce origen opaco;
    /// un `src` http(s) produce su propio origen.
    pub fn add_iframe(&mut self, name: &str, src: &str, html: &str) -> &Document {
        let document = if src.is_empty() || src == "about:blank" {
            Document {
                url: src.to_owned(),
                origin: Origin::Opaque,
                title: title_from_html(html),
                html: html.to_owned(),
                written: String::new(),
            }
        } else {
            Document::new(src, html)
        };
        self.iframes.insert(name.to_owned(), document);
        self.iframes.get(name).expect("iframe recién insertado")
    }

    pub fn iframe(&self, name: &str) -> Option<&Document> {
        self.iframes.get(name)
    }

    pub fn iframe_count(&self) -> usize {
        self.iframes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_origin_is_never_same_origin() {
        assert!(!Origin::Opaque.same_origin(&Origin::Opaque));
        let http = Origin::from_url("https://example.com/a");
        assert!(!http.same_origin(&Origin::Opaque));
        assert!(http.same_origin(&Origin::from_url("https://example.com/b")));
        assert!(!http.same_origin(&Origin::from_url("https://other.com/")));
        assert!(!http.same_origin(&Origin::from_url("http://example.com/")));
    }

    #[test]
    fn document_write_appends_to_the_stream() {
        let mut doc = Document::new("https://example.com/", "<html><body>");
        doc.write("<p>hola</p>");
        assert_eq!(doc.document_html(), "<html><body><p>hola</p>");
        assert_eq!(doc.title(), "");
    }

    #[test]
    fn navigable_cuts_the_future_on_navigate() {
        let mut nav = Navigable::new("https://a/");
        nav.navigate("https://b/");
        nav.navigate("https://c/");
        assert_eq!(nav.back(), Some("https://b/"));
        nav.navigate("https://d/");
        assert_eq!(nav.forward(), None);
        assert_eq!(nav.history(), &["https://a/", "https://b/", "https://d/"]);
    }

    #[test]
    fn iframe_about_blank_is_opaque() {
        let mut page = Page::load("https://example.com/", "<html></html>");
        let frame = page.add_iframe("f", "about:blank", "");
        assert_eq!(frame.origin(), &Origin::Opaque);
        assert_eq!(page.iframe_count(), 1);
    }
}
