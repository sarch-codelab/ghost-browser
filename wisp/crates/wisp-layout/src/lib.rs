//! Motor de maquetación propio de Wisp (ítem 12 del backlog).
//!
//! Define los cuatro contextos de formato (BFC, IFC, FFC, TFC), las cajas de
//! layout (incluidas las cajas anónimas cuando un contenedor de bloque mezcla
//! hijos inline y bloque), `LayoutState` con valores usados, colapso de
//! márgenes CSS 2.1 y ruptura de líneas básica para IFC.
//!
//! Vive en paralelo mientras `wisp-engine` sigue usando `taffy`, para ir
//! migrando fixture a fixture sin romper el navegador.

#![forbid(unsafe_code)]

use std::collections::HashMap;

/// Contexto de formato de una caja de layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FormattingContext {
    #[default]
    Block,
    Inline,
    Flex,
    Table,
}

/// Tipo de caja en el árbol de layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxKind {
    Block,
    Inline,
    AnonymousBlock,
    Flex,
    Table,
    TextRun,
}

/// Rectángulo geométrico en coordenadas CSS (píxeles).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LayoutRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Valores usados resueltos para una caja tras el layout (`LayoutState`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct UsedValues {
    pub border_box: LayoutRect,
    pub content_box: LayoutRect,
    pub margin: [f32; 4],
    pub padding: [f32; 4],
    pub border: [f32; 4],
}

/// Estado completo de una pasada de layout, indexado por id de caja.
#[derive(Debug, Default)]
pub struct LayoutState {
    pub used: HashMap<usize, UsedValues>,
}

impl LayoutState {
    pub fn record(&mut self, id: usize, values: UsedValues) {
        self.used.insert(id, values);
    }

    pub fn get(&self, id: usize) -> Option<&UsedValues> {
        self.used.get(&id)
    }
}

/// Entrada de caja antes de normalizar cajas anónimas.
#[derive(Debug, Clone)]
pub struct LayoutNode {
    pub id: usize,
    pub kind: BoxKind,
    pub formatting_context: FormattingContext,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub margin: [f32; 4],
    pub padding: [f32; 4],
    pub border: [f32; 4],
    pub text: String,
    pub font_size: f32,
    pub line_height: f32,
    pub flex_grow: f32,
    pub children: Vec<LayoutNode>,
}

impl LayoutNode {
    pub fn block(id: usize) -> Self {
        Self {
            id,
            kind: BoxKind::Block,
            formatting_context: FormattingContext::Block,
            width: None,
            height: None,
            margin: [0.0; 4],
            padding: [0.0; 4],
            border: [0.0; 4],
            text: String::new(),
            font_size: 16.0,
            line_height: 22.0,
            flex_grow: 0.0,
            children: Vec::new(),
        }
    }

    pub fn inline_text(id: usize, text: impl Into<String>, font_size: f32) -> Self {
        Self {
            id,
            kind: BoxKind::TextRun,
            formatting_context: FormattingContext::Inline,
            width: None,
            height: None,
            margin: [0.0; 4],
            padding: [0.0; 4],
            border: [0.0; 4],
            text: text.into(),
            font_size,
            line_height: (font_size * 1.35).ceil(),
            flex_grow: 0.0,
            children: Vec::new(),
        }
    }
}

/// Regla CSS 2.1 §8.3.1 de colapso de márgenes verticales adyacentes:
/// - dos positivos -> el máximo
/// - dos negativos -> el más negativo (mínimo)
/// - signos opuestos -> suma algebraica
pub fn collapse_margins(prev_bottom: f32, next_top: f32) -> f32 {
    if prev_bottom >= 0.0 && next_top >= 0.0 {
        prev_bottom.max(next_top)
    } else if prev_bottom <= 0.0 && next_top <= 0.0 {
        prev_bottom.min(next_top)
    } else {
        prev_bottom + next_top
    }
}

/// Normaliza un contenedor de bloque: si mezcla cajas de bloque e inline,
/// envuelve las secuencias contiguas de hijos inline en `BoxKind::AnonymousBlock`.
pub fn normalize_block_children(
    children: Vec<LayoutNode>,
    next_anon_id: &mut usize,
) -> Vec<LayoutNode> {
    let has_block = children.iter().any(|c| {
        matches!(
            c.kind,
            BoxKind::Block | BoxKind::Flex | BoxKind::Table | BoxKind::AnonymousBlock
        )
    });
    let has_inline = children
        .iter()
        .any(|c| matches!(c.kind, BoxKind::Inline | BoxKind::TextRun));

    if !(has_block && has_inline) {
        return children;
    }

    let mut out = Vec::with_capacity(children.len());
    let mut inline_run: Vec<LayoutNode> = Vec::new();

    for child in children {
        if matches!(child.kind, BoxKind::Inline | BoxKind::TextRun) {
            inline_run.push(child);
        } else {
            if !inline_run.is_empty() {
                out.push(wrap_anonymous_block(
                    std::mem::take(&mut inline_run),
                    next_anon_id,
                ));
            }
            out.push(child);
        }
    }
    if !inline_run.is_empty() {
        out.push(wrap_anonymous_block(inline_run, next_anon_id));
    }
    out
}

fn wrap_anonymous_block(children: Vec<LayoutNode>, next_anon_id: &mut usize) -> LayoutNode {
    let id = *next_anon_id;
    *next_anon_id += 1;
    LayoutNode {
        id,
        kind: BoxKind::AnonymousBlock,
        formatting_context: FormattingContext::Block,
        width: None,
        height: None,
        margin: [0.0; 4],
        padding: [0.0; 4],
        border: [0.0; 4],
        text: String::new(),
        font_size: 16.0,
        line_height: 22.0,
        flex_grow: 0.0,
        children,
    }
}

/// Caja de línea producida por un contexto IFC.
#[derive(Debug, Clone, PartialEq)]
pub struct LineBox {
    pub y: f32,
    pub height: f32,
    pub text: String,
    pub width: f32,
}

/// Ejecuta la maquetación BFC/IFC/FFC/TFC de un árbol `LayoutNode` dentro de
/// `available_width` y devuelve el `LayoutState` con los valores usados.
pub fn layout_tree(root: &LayoutNode, available_width: f32) -> LayoutState {
    let mut state = LayoutState::default();
    let mut next_anon_id = 1_000_000usize;
    layout_block(root, 0.0, 0.0, available_width, &mut next_anon_id, &mut state);
    state
}

fn layout_block(
    node: &LayoutNode,
    origin_x: f32,
    origin_y: f32,
    containing_width: f32,
    next_anon_id: &mut usize,
    state: &mut LayoutState,
) -> f32 {
    let outer_x = origin_x + node.margin[3];
    let outer_y = origin_y + node.margin[0];

    let horizontal_chrome = node.margin[1]
        + node.margin[3]
        + node.padding[1]
        + node.padding[3]
        + node.border[1]
        + node.border[3];
    let border_width = node
        .width
        .map(|w| w + node.padding[1] + node.padding[3] + node.border[1] + node.border[3])
        .unwrap_or_else(|| (containing_width - node.margin[1] - node.margin[3]).max(0.0));
    let content_width = node
        .width
        .unwrap_or_else(|| (containing_width - horizontal_chrome).max(0.0));

    let content_x = outer_x + node.border[3] + node.padding[3];
    let content_y = outer_y + node.border[0] + node.padding[0];

    let children = normalize_block_children(node.children.clone(), next_anon_id);
    let inner_height = match node.formatting_context {
        FormattingContext::Flex => {
            layout_flex_children(&children, content_x, content_y, content_width, next_anon_id, state)
        }
        FormattingContext::Table => layout_table_children(
            &children,
            content_x,
            content_y,
            content_width,
            next_anon_id,
            state,
        ),
        FormattingContext::Inline => {
            let lines = break_inline_lines(&children, content_width);
            lines.iter().map(|l| l.height).sum()
        }
        FormattingContext::Block => {
            if children.iter().all(|c| matches!(c.kind, BoxKind::Inline | BoxKind::TextRun)) {
                let lines = break_inline_lines(&children, content_width);
                lines.iter().map(|l| l.height).sum()
            } else {
                let mut cursor_y = content_y;
                let mut prev_margin_bottom = 0.0f32;
                let mut first = true;
                for child in &children {
                    let gap = if first {
                        first = false;
                        child.margin[0]
                    } else {
                        collapse_margins(prev_margin_bottom, child.margin[0])
                    };
                    cursor_y += gap - child.margin[0];
                    let used_h = layout_block(
                        child,
                        content_x,
                        cursor_y,
                        content_width,
                        next_anon_id,
                        state,
                    );
                    cursor_y += used_h;
                    prev_margin_bottom = child.margin[2];
                }
                (cursor_y - content_y).max(0.0)
            }
        }
    };

    let content_height = node.height.unwrap_or(inner_height);
    let border_height =
        content_height + node.padding[0] + node.padding[2] + node.border[0] + node.border[2];

    state.record(
        node.id,
        UsedValues {
            border_box: LayoutRect {
                x: outer_x,
                y: outer_y,
                width: border_width,
                height: border_height,
            },
            content_box: LayoutRect {
                x: content_x,
                y: content_y,
                width: content_width,
                height: content_height,
            },
            margin: node.margin,
            padding: node.padding,
            border: node.border,
        },
    );

    node.margin[0] + border_height + node.margin[2]
}

fn layout_flex_children(
    children: &[LayoutNode],
    content_x: f32,
    content_y: f32,
    content_width: f32,
    next_anon_id: &mut usize,
    state: &mut LayoutState,
) -> f32 {
    if children.is_empty() {
        return 0.0;
    }
    let total_grow: f32 = children.iter().map(|c| c.flex_grow).sum();
    let fixed_sum: f32 = children.iter().filter_map(|c| c.width).sum();
    let remaining = (content_width - fixed_sum).max(0.0);
    let default_share = content_width / children.len() as f32;

    let mut x = content_x;
    let mut max_h = 0.0f32;
    for child in children {
        let w = if let Some(explicit) = child.width {
            explicit
        } else if total_grow > 0.0 {
            remaining * (child.flex_grow / total_grow)
        } else {
            default_share
        };
        let mut adjusted = child.clone();
        adjusted.width = Some(w);
        let h = layout_block(&adjusted, x, content_y, w, next_anon_id, state);
        x += w;
        max_h = max_h.max(h);
    }
    max_h
}

fn layout_table_children(
    rows: &[LayoutNode],
    content_x: f32,
    content_y: f32,
    content_width: f32,
    next_anon_id: &mut usize,
    state: &mut LayoutState,
) -> f32 {
    let mut y = content_y;
    for row in rows {
        let row_h =
            layout_flex_children(&row.children, content_x, y, content_width, next_anon_id, state);
        y += row_h;
    }
    (y - content_y).max(0.0)
}

/// Rompe el texto de los hijos inline en `LineBox` respetando `max_width`.
pub fn break_inline_lines(children: &[LayoutNode], max_width: f32) -> Vec<LineBox> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0.0f32;
    let mut cur_h = 0.0f32;
    let mut y = 0.0f32;

    for child in children {
        let char_w = child.font_size * 0.52;
        let line_h = child.line_height.max(child.font_size * 1.25);
        for word in child.text.split_whitespace() {
            let word_w = word.chars().count() as f32 * char_w;
            let space_w = if cur.is_empty() { 0.0 } else { char_w };
            if !cur.is_empty() && cur_w + space_w + word_w > max_width {
                lines.push(LineBox {
                    y,
                    height: cur_h,
                    text: std::mem::take(&mut cur),
                    width: cur_w,
                });
                y += cur_h;
                cur_w = 0.0;
                cur_h = 0.0;
            }
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += char_w;
            }
            cur.push_str(word);
            cur_w += word_w;
            cur_h = cur_h.max(line_h);
        }
    }
    if !cur.is_empty() {
        lines.push(LineBox {
            y,
            height: cur_h,
            text: cur,
            width: cur_w,
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_positive_negative_and_mixed_margins() {
        assert_eq!(collapse_margins(16.0, 24.0), 24.0);
        assert_eq!(collapse_margins(-10.0, -25.0), -25.0);
        assert_eq!(collapse_margins(20.0, -8.0), 12.0);
    }

    #[test]
    fn wraps_inline_runs_into_anonymous_blocks() {
        let children = vec![
            LayoutNode::inline_text(1, "hola", 16.0),
            LayoutNode::block(2),
            LayoutNode::inline_text(3, "mundo", 16.0),
        ];
        let mut next_id = 100;
        let normalized = normalize_block_children(children, &mut next_id);
        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[0].kind, BoxKind::AnonymousBlock);
        assert_eq!(normalized[1].kind, BoxKind::Block);
        assert_eq!(normalized[2].kind, BoxKind::AnonymousBlock);
    }

    #[test]
    fn bfc_collapses_sibling_margins_and_records_layout_state() {
        let mut a = LayoutNode::block(2);
        a.height = Some(20.0);
        a.margin = [0.0, 0.0, 16.0, 0.0];

        let mut b = LayoutNode::block(3);
        b.height = Some(30.0);
        b.margin = [24.0, 0.0, 0.0, 0.0];

        let mut root = LayoutNode::block(1);
        root.children = vec![a, b];

        let state = layout_tree(&root, 800.0);
        let used_b = state.get(3).expect("caja b registrada");
        assert_eq!(used_b.border_box.y, 44.0); // 20 + max(16, 24)
        assert_eq!(state.get(1).expect("root").content_box.height, 74.0);
    }
}
