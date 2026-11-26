//! Codificador PNG mínimo, sin dependencias: PNG «stored» (deflate sin
//! compresión) sobre color verdadero de 8 bits (tipo 2). Es suficiente para
//! las capturas de `wisp-agent` y evita arrastrar un compresor entero al
//! binario headless. El formato va con `filter: 0` por línea, bloques deflate
//! `BTYPE=00` (crudo) y los CRC/Adler-32 que exige el estándar.

/// Tabla de CRC-32 (polinomio IEEE 0xEDB88320) generada en compilación.
const CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.0 = CRC_TABLE[((self.0 ^ byte as u32) & 0xFF) as usize] ^ (self.0 >> 8);
        }
    }

    fn finish(self) -> u32 {
        !self.0
    }
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Envuelve el deflate en un flujo zlib (`0x78 0x01` + datos + Adler-32).
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65_535 * 5 + 16);
    out.extend_from_slice(&[0x78, 0x01]);
    if data.is_empty() {
        // Un bloque final vacío es obligatorio: zlib necesita al menos uno.
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut blocks = data.chunks(65_535).peekable();
        while let Some(block) = blocks.next() {
            out.push(if blocks.peek().is_none() { 0x01 } else { 0x00 });
            let len = block.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(block);
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

/// Codifica `pixels` (RGB entrelazado, `width * height * 3` bytes) como PNG.
pub fn encode_rgb(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    debug_assert_eq!(pixels.len(), width as usize * height as usize * 3);
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity(height as usize * (1 + stride));
    for row in pixels.chunks_exact(stride) {
        raw.push(0); // filtro «None» por línea
        raw.extend_from_slice(row);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8 bits, truecolor RGB, sin interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_png_with_the_expected_signature_and_size() {
        // 2×2 rojo, verde, azul, blanco.
        let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let png = encode_rgb(2, 2, &pixels);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // El IHDR va primero y declara las dimensiones en big-endian.
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 2);
        assert!(png.ends_with(&[
            0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82
        ]));
    }

    #[test]
    fn zlib_stored_splits_oversized_input_into_legal_blocks() {
        // Más de un bloque de 65535 bytes: cada bloque debe seguir siendo legal.
        let data = vec![0xABu8; 70_000];
        let zlib = zlib_stored(&data);
        assert_eq!(&zlib[..2], &[0x78, 0x01]);
        assert_eq!(
            u32::from_be_bytes(zlib[zlib.len() - 4..].try_into().unwrap()),
            adler32(&data)
        );
    }
}
