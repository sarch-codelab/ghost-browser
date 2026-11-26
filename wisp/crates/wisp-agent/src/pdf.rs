//! Generador mínimo de PDF 1.4 para `browser_pdf` y `Page.printToPDF`.
//!
//! Cero dependencias externas: emite un PDF válido con texto maquetado en
//! múltiples páginas (Helvetica Type1) y tabla xref con offsets exactos.

use wisp_engine::{LayoutDocument, PaintCommand};

/// Renderiza un `LayoutDocument` (y su título) a bytes `%PDF-1.4`.
pub fn render_pdf(title: &str, document: &LayoutDocument) -> Vec<u8> {
    let mut lines: Vec<String> = Vec::new();
    if !title.trim().is_empty() {
        lines.push(title.trim().to_owned());
        lines.push(String::new());
    }
    let mut current_y: Option<f32> = None;
    let mut line_buf = String::new();
    for cmd in &document.commands {
        if let PaintCommand::Text { y, content, .. } = cmd {
            if let Some(prev_y) = current_y
                && (*y - prev_y).abs() > 4.0
            {
                let trimmed = line_buf.trim().to_owned();
                if !trimmed.is_empty() {
                    lines.push(trimmed);
                }
                line_buf.clear();
            }
            current_y = Some(*y);
            line_buf.push_str(content);
        }
    }
    let tail = line_buf.trim().to_owned();
    if !tail.is_empty() {
        lines.push(tail);
    }
    if lines.is_empty() {
        lines.push("(página vacía)".to_owned());
    }

    // Divide en páginas de 48 líneas (~720 pt útiles sobre 792 pt).
    let chunks: Vec<&[String]> = lines.chunks(48).collect();
    let page_count = chunks.len().max(1);

    let mut objects: Vec<Vec<u8>> = Vec::new();
    // Obj 1: Catalog
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    // Obj 2: Pages (kids = 4, 6, 8...)
    let kids: Vec<String> = (0..page_count).map(|i| format!("{} 0 R", 4 + i * 2)).collect();
    objects.push(format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.join(" "), page_count).into_bytes());
    // Obj 3: Font Helvetica
    objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());

    for (i, chunk) in chunks.iter().enumerate() {
        let content_obj_id = 5 + i * 2;
        let page_obj = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {content_obj_id} 0 R >>"
        );
        objects.push(page_obj.into_bytes());

        let mut stream = String::from("BT\n/F1 11 Tf\n48 744 Td\n14 TL\n");
        for (line_idx, line) in chunk.iter().enumerate() {
            let escaped = pdf_escape(line);
            if line_idx == 0 {
                stream.push_str(&format!("({escaped}) Tj\n"));
            } else {
                stream.push_str(&format!("T* ({escaped}) Tj\n"));
            }
        }
        stream.push_str("ET\n");
        let content_obj = format!(
            "<< /Length {} >>\nstream\n{}endstream",
            stream.len(),
            stream
        );
        objects.push(content_obj.into_bytes());
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    let mut offsets = Vec::with_capacity(objects.len() + 1);
    offsets.push(0usize);

    for (idx, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", idx + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }

    let xref_offset = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in offsets.iter().skip(1) {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

fn pdf_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars().take(120) {
        match ch {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            _ => out.push(' '),
        }
    }
    out
}
