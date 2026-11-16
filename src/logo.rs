//! Logotipo de Ghost compartido por la interfaz y la ventana.

use image::imageops::FilterType;
use image::{RgbaImage, load_from_memory};

const GHOST_MARK: &[u8] = include_bytes!("../ghost-mark.png");

/// Genera la marca de Ghost como imagen RGBA del tamaño pedido.
pub fn icon(size: u32) -> RgbaImage {
    let size = size.max(16);
    let source = load_from_memory(GHOST_MARK)
        .expect("la marca de Ghost debe ser un PNG válido")
        .into_rgba8();
    image::imageops::resize(&source, size, size, FilterType::Lanczos3)
}

/// Icono de ventana / barra de tareas.
pub fn window_icon() -> Option<winit::window::Icon> {
    let img = icon(64);
    let (w, h) = (img.width(), img.height());
    winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

pub fn compact_icon_png() -> &'static [u8] {
    GHOST_MARK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genera_icono_opaco() {
        let img = icon(32);
        assert_eq!(img.dimensions(), (32, 32));
        assert!(img.get_pixel(0, 0)[3] < 255);
        assert_eq!(img.get_pixel(16, 16)[3], 255);
    }

    #[test]
    fn icono_compacto_tiene_silueta_y_transparencia() {
        let img = icon(64);
        assert_eq!(img.dimensions(), (64, 64));
        assert_eq!(img.get_pixel(32, 40)[3], 255);
        assert_eq!(img.get_pixel(0, 0)[3], 0);
        assert_eq!(img.get_pixel(26, 31)[3], 0);
    }

    #[test]
    fn exporta_la_vista_previa_compacta_como_png() {
        let bytes = compact_icon_png();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let image = load_from_memory(bytes).unwrap();
        assert_eq!((image.width(), image.height()), (256, 256));
    }
}
