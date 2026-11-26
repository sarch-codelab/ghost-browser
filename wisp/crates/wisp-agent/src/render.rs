//! Rasterizador de la lista de pintura del motor: convierte un
//! [`LayoutDocument`] en un PNG. Es la pieza que le falta a `wisp-agent` para
//! que un agente pueda **ver** la página y saber qué coordenadas usar con
//! `click`/`move`.
//!
//! Dibuja lo que el motor describe (fondos, bordes, texto y degradados) sobre
//! un lienzo blanco. Dos aproximaciones deliberadas, por coste y por no añadir
//! decodificadores de imagen al binario: las `background-image` con `url(...)` y
//! las imágenes se pintan como un marcador gris, y los radios de esquina se
//! ignoran.

use ab_glyph::{Font as _, FontArc, ScaleFont as _, point};
use anyhow::Result;
use std::sync::OnceLock;
use wisp_engine::{BgImage, Color, GradientStop, LayoutDocument, PaintCommand, Rect};

const LIGHT_FONT: &[u8] = include_bytes!("../../../assets/fonts/Ubuntu-Light.ttf");
const BOLD_FONT: &[u8] = include_bytes!("../../../assets/fonts/Ubuntu-Bold.ttf");
/// Tope de píxeles del lienzo: una página muy larga se recorta a esta altura
/// en vez de reservar cientos de MB para una captura.
const MAX_CANVAS_PIXELS: u64 = 8_000_000;
const PLACEHOLDER_FILL: Color = Color {
    r: 235,
    g: 235,
    b: 235,
    a: 255,
};
const PLACEHOLDER_BORDER: Color = Color {
    r: 200,
    g: 200,
    b: 200,
    a: 255,
};

/// PNG resultante y dimensiones del lienzo, para que el agente sepa en qué
/// espacio de coordenadas está.
pub struct Screenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Rasteriza la página a `viewport_width` y devuelve el PNG.
pub fn render(page: &wisp_engine::FetchedPage, viewport_width: f32) -> Result<Screenshot> {
    let document = page.layout(viewport_width)?;
    render_document(&document)
}

fn render_document(document: &LayoutDocument) -> Result<Screenshot> {
    let width = document.viewport_width.max(1.0).round() as u32;
    let max_height = (MAX_CANVAS_PIXELS / width.max(1) as u64).max(1) as u32;
    let height = (document.content_height.max(1.0).ceil() as u32).min(max_height);

    let mut canvas = Canvas::new(width, height);
    for command in &document.commands {
        match command {
            PaintCommand::FillRect { rect, color, .. } => canvas.fill_rect(*rect, *color),
            PaintCommand::StrokeRect {
                rect, color, width, ..
            } => canvas.stroke_rect(*rect, *color, *width),
            PaintCommand::Text {
                x,
                y,
                content,
                color,
                font_size,
                font_weight,
                ..
            } => canvas.draw_text(*x, *y, content, *color, *font_size, *font_weight >= 600),
            PaintCommand::Image { rect, .. } => canvas.placeholder(*rect),
            PaintCommand::BackgroundImage { rect, image, .. } => match image {
                BgImage::LinearGradient { angle_deg, stops } => {
                    canvas.fill_gradient(*rect, *angle_deg, stops)
                }
                BgImage::Url(_) => canvas.placeholder(*rect),
            },
            PaintCommand::Clip { rect } => canvas.push_clip(*rect),
            PaintCommand::ClipEnd => canvas.pop_clip(),
        }
    }

    Ok(Screenshot {
        png: crate::png::encode_rgb(width, height, &canvas.pixels),
        width,
        height,
    })
}

fn fonts() -> (&'static FontArc, &'static FontArc) {
    static LIGHT: OnceLock<FontArc> = OnceLock::new();
    static BOLD: OnceLock<FontArc> = OnceLock::new();
    (
        LIGHT.get_or_init(|| {
            FontArc::try_from_slice(LIGHT_FONT).expect("la fuente Ubuntu-Light debe ser válida")
        }),
        BOLD.get_or_init(|| {
            FontArc::try_from_slice(BOLD_FONT).expect("la fuente Ubuntu-Bold debe ser válida")
        }),
    )
}

struct Canvas {
    width: u32,
    height: u32,
    /// RGB entrelazado; el fondo arranca en blanco.
    pixels: Vec<u8>,
    clip: Option<Rect>,
    clip_stack: Vec<Option<Rect>>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![255; width as usize * height as usize * 3],
            clip: None,
            clip_stack: Vec::new(),
        }
    }

    fn push_clip(&mut self, rect: Rect) {
        self.clip_stack.push(self.clip);
        self.clip = Some(match self.clip {
            Some(current) => intersect_rect(current, rect),
            None => rect,
        });
    }

    fn pop_clip(&mut self) {
        self.clip = self.clip_stack.pop().flatten();
    }

    /// Mezcla `color` en (`x`, `y`) con la cobertura dada, respetando el
    /// recorte activo. Fuera del lienzo o del recorte no hace nada.
    fn blend(&mut self, x: i32, y: i32, color: Color, coverage: f32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        if let Some(clip) = self.clip {
            let (fx, fy) = (x as f32, y as f32);
            let inside = fx >= clip.x
                && fx <= clip.x + clip.width
                && fy >= clip.y
                && fy <= clip.y + clip.height;
            if !inside {
                return;
            }
        }
        let alpha = (color.a as f32 / 255.0) * coverage.clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return;
        }
        let index = (y as usize * self.width as usize + x as usize) * 3;
        let destination = &mut self.pixels[index..index + 3];
        for (channel, source) in [color.r, color.g, color.b].into_iter().enumerate() {
            let value = source as f32 * alpha + destination[channel] as f32 * (1.0 - alpha);
            destination[channel] = value.round().clamp(0.0, 255.0) as u8;
        }
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return;
        }
        let x0 = rect.x.floor().max(0.0) as i32;
        let y0 = rect.y.floor().max(0.0) as i32;
        let x1 = (rect.x + rect.width).ceil().min(self.width as f32) as i32;
        let y1 = (rect.y + rect.height).ceil().min(self.height as f32) as i32;
        for y in y0..y1 {
            for x in x0..x1 {
                self.blend(x, y, color, 1.0);
            }
        }
    }

    fn stroke_rect(&mut self, rect: Rect, color: Color, width: f32) {
        let w = width.max(1.0);
        self.fill_rect(
            Rect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: w,
            },
            color,
        );
        self.fill_rect(
            Rect {
                x: rect.x,
                y: rect.y + rect.height - w,
                width: rect.width,
                height: w,
            },
            color,
        );
        self.fill_rect(
            Rect {
                x: rect.x,
                y: rect.y,
                width: w,
                height: rect.height,
            },
            color,
        );
        self.fill_rect(
            Rect {
                x: rect.x + rect.width - w,
                y: rect.y,
                width: w,
                height: rect.height,
            },
            color,
        );
    }

    fn placeholder(&mut self, rect: Rect) {
        self.fill_rect(rect, PLACEHOLDER_FILL);
        self.stroke_rect(rect, PLACEHOLDER_BORDER, 1.0);
    }

    fn draw_text(
        &mut self,
        x: f32,
        y: f32,
        content: &str,
        color: Color,
        font_size: f32,
        bold: bool,
    ) {
        if content.is_empty() || font_size <= 0.0 {
            return;
        }
        let (light, bold_font) = fonts();
        let font = if bold { bold_font } else { light };
        let scaled = font.as_scaled(font_size);
        // `y` es el borde superior de la línea; la línea base baja un ascenso.
        let baseline = y + scaled.ascent();
        let mut pen_x = x;
        let mut previous = None;
        for character in content.chars() {
            let glyph_id = scaled.glyph_id(character);
            if let Some(previous) = previous {
                pen_x += scaled.kern(previous, glyph_id);
            }
            let glyph = glyph_id.with_scale_and_position(font_size, point(pen_x, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    self.blend(px, py, color, coverage);
                });
            }
            pen_x += scaled.h_advance(glyph_id);
            previous = Some(glyph_id);
        }
    }

    /// Degradado lineal siguiendo la convención CSS (0° = hacia arriba, 90° =
    /// hacia la derecha): proyecta cada píxel sobre el eje y reparte las
    /// paradas a lo largo de ese eje.
    fn fill_gradient(&mut self, rect: Rect, angle_deg: f32, stops: &[GradientStop]) {
        if rect.width <= 0.0 || rect.height <= 0.0 || stops.is_empty() {
            return;
        }
        let angle = angle_deg.to_radians();
        let (dir_x, dir_y) = (angle.sin(), -angle.cos());
        let corners = [
            (rect.x, rect.y),
            (rect.x + rect.width, rect.y),
            (rect.x, rect.y + rect.height),
            (rect.x + rect.width, rect.y + rect.height),
        ];
        let project = |x: f32, y: f32| x * dir_x + y * dir_y;
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for (x, y) in corners {
            min = min.min(project(x, y));
            max = max.max(project(x, y));
        }
        let span = (max - min).max(f32::EPSILON);

        let x0 = rect.x.floor().max(0.0) as i32;
        let y0 = rect.y.floor().max(0.0) as i32;
        let x1 = (rect.x + rect.width).ceil().min(self.width as f32) as i32;
        let y1 = (rect.y + rect.height).ceil().min(self.height as f32) as i32;
        for y in y0..y1 {
            for x in x0..x1 {
                let t = (project(x as f32 + 0.5, y as f32 + 0.5) - min) / span;
                self.blend(x, y, sample_gradient(stops, t), 1.0);
            }
        }
    }
}

fn intersect_rect(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.width).min(b.x + b.width);
    let y1 = (a.y + a.height).min(b.y + b.height);
    Rect {
        x: x0,
        y: y0,
        width: (x1 - x0).max(0.0),
        height: (y1 - y0).max(0.0),
    }
}

fn sample_gradient(stops: &[GradientStop], t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let mut previous = stops[0];
    if t <= previous.offset {
        return previous.color;
    }
    for &stop in &stops[1..] {
        if t <= stop.offset {
            let span = (stop.offset - previous.offset).max(f32::EPSILON);
            return lerp_color(previous.color, stop.color, (t - previous.offset) / span);
        }
        previous = stop;
    }
    previous.color
}

fn lerp_color(from: Color, to: Color, t: f32) -> Color {
    let mix = |a: u8, b: u8| {
        (a as f32 + (b as f32 - a as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color {
        r: mix(from.r, to.r),
        g: mix(from.g, to.g),
        b: mix(from.b, to.b),
        a: mix(from.a, to.a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(canvas: &Canvas, x: u32, y: u32) -> [u8; 3] {
        let index = (y as usize * canvas.width as usize + x as usize) * 3;
        canvas.pixels[index..index + 3].try_into().unwrap()
    }

    #[test]
    fn fill_rect_paints_the_requested_color() {
        let mut canvas = Canvas::new(4, 4);
        canvas.fill_rect(
            Rect {
                x: 1.0,
                y: 1.0,
                width: 2.0,
                height: 2.0,
            },
            Color {
                r: 255,
                g: 0,
                b: 0,
                a: 255,
            },
        );
        assert_eq!(pixel(&canvas, 1, 1), [255, 0, 0]);
        assert_eq!(pixel(&canvas, 2, 2), [255, 0, 0]);
        assert_eq!(pixel(&canvas, 0, 0), [255, 255, 255]);
    }

    #[test]
    fn clip_keeps_paint_inside_the_region() {
        let mut canvas = Canvas::new(4, 4);
        canvas.push_clip(Rect {
            x: 0.0,
            y: 0.0,
            width: 2.0,
            height: 2.0,
        });
        canvas.fill_rect(
            Rect {
                x: 0.0,
                y: 0.0,
                width: 4.0,
                height: 4.0,
            },
            Color {
                r: 0,
                g: 0,
                b: 255,
                a: 255,
            },
        );
        canvas.pop_clip();
        assert_eq!(pixel(&canvas, 1, 1), [0, 0, 255]);
        assert_eq!(pixel(&canvas, 3, 3), [255, 255, 255]);
    }

    #[test]
    fn renders_a_layout_to_a_png_of_the_expected_size() {
        let document = wisp_engine::layout_html(
            r#"<html><body style="margin:0"><p style="color:#ff0000">hola</p></body></html>"#,
            800.0,
        )
        .unwrap();
        let shot = render_document(&document).unwrap();
        assert_eq!(shot.width, 800);
        assert!(shot.height >= 1);
        assert_eq!(
            &shot.png[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        // El texto rojo debe haber pintado algo: sin dibujo, el PNG sería
        // idéntico a un lienzo en blanco del mismo tamaño.
        let blank = crate::png::encode_rgb(
            shot.width,
            shot.height,
            &vec![255u8; shot.width as usize * shot.height as usize * 3],
        );
        assert_ne!(
            shot.png, blank,
            "la captura debería diferir de un lienzo en blanco"
        );
    }
}
