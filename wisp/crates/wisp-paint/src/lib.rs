//! Paint list de Wisp: las primitivas geométricas y la lista de comandos que
//! el motor produce y un host (egui en Ghost, una CLI headless en `wisp-agent`)
//! consume. Este crate no conoce el DOM ni el estilo: solo describe qué pintar.

use serde::Serialize;

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
        /// Resolved URL (or full `data:` URI) used as the key into the
        /// fetched page's `images` map.
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
