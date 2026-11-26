//! `StyleComputer` con buckets por selector más a la derecha (ítem 11).
//!
//! Indexa las reglas de [`wisp_css::CssSheet`] por `#id`, `.clase`, `etiqueta`
//! o `*` según el componente más a la derecha del selector, evitando probar
//! todas las reglas de una hoja grande (p. ej. Wikipedia) contra cada nodo.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use wisp_css::{CssRule, CssSheet, Declaration, Origin, Specificity, parse_declarations};

/// Vista mínima de un elemento para resolver selectores sin acoplarse al DOM.
pub trait ElementView {
    fn tag_name(&self) -> &str;
    fn id(&self) -> Option<&str>;
    fn has_class(&self, class: &str) -> bool;
    fn attribute(&self, name: &str) -> Option<&str>;
    fn parent(&self) -> Option<&Self>;
}

/// Elemento en memoria útil para tests y adaptadores rápidos.
#[derive(Debug, Clone, Default)]
pub struct SimpleElement {
    pub tag: String,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attributes: HashMap<String, String>,
    pub parent: Option<Box<SimpleElement>>,
}

impl ElementView for SimpleElement {
    fn tag_name(&self) -> &str {
        &self.tag
    }

    fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    fn has_class(&self, class: &str) -> bool {
        self.classes.iter().any(|c| c == class)
    }

    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str)
    }

    fn parent(&self) -> Option<&Self> {
        self.parent.as_deref()
    }
}

/// Motor de resolución de estilos indexado por buckets.
#[derive(Debug, Default)]
pub struct StyleComputer {
    rules: Vec<CssRule>,
    by_id: HashMap<String, Vec<usize>>,
    by_class: HashMap<String, Vec<usize>>,
    by_tag: HashMap<String, Vec<usize>>,
    universal: Vec<usize>,
}

impl StyleComputer {
    /// Construye un `StyleComputer` con la hoja UA seguida de las hojas de autor.
    pub fn from_sheets(sheets: &[CssSheet], viewport_width: f32, supported: &[&str]) -> Self {
        let mut computer = Self::default();
        computer.add_sheet(&CssSheet::user_agent(), viewport_width, supported);
        for sheet in sheets {
            computer.add_sheet(sheet, viewport_width, supported);
        }
        computer
    }

    /// Añade una hoja completa al índice de buckets.
    pub fn add_sheet(&mut self, sheet: &CssSheet, viewport_width: f32, supported: &[&str]) {
        for rule in sheet.flatten_for_viewport(viewport_width, supported) {
            self.add_rule(rule);
        }
    }

    /// Inserta e indexa una regla individual por el componente más a la derecha.
    pub fn add_rule(&mut self, rule: CssRule) {
        let idx = self.rules.len();
        match rightmost_bucket_key(&rule.selector) {
            BucketKey::Id(id) => self.by_id.entry(id).or_default().push(idx),
            BucketKey::Class(cls) => self.by_class.entry(cls).or_default().push(idx),
            BucketKey::Tag(tag) => self.by_tag.entry(tag).or_default().push(idx),
            BucketKey::Universal => self.universal.push(idx),
        }
        self.rules.push(rule);
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Devuelve únicamente los índices candidatos que pueden casar con `el`.
    pub fn candidate_indices<E: ElementView>(&self, el: &E) -> Vec<usize> {
        let mut candidates = Vec::new();
        if let Some(id) = el.id()
            && let Some(bucket) = self.by_id.get(id)
        {
            candidates.extend_from_slice(bucket);
        }
        if !self.by_class.is_empty() {
            // Comprobamos clases presentes si las expone `attribute("class")` o
            // recorremos las clases del bucket cuando el elemento no expone lista.
            if let Some(cls_attr) = el.attribute("class") {
                for cls in cls_attr.split_ascii_whitespace() {
                    if let Some(bucket) = self.by_class.get(cls) {
                        candidates.extend_from_slice(bucket);
                    }
                }
            } else {
                for (cls, bucket) in &self.by_class {
                    if el.has_class(cls) {
                        candidates.extend_from_slice(bucket);
                    }
                }
            }
        }
        let tag = el.tag_name().to_ascii_lowercase();
        if let Some(bucket) = self.by_tag.get(&tag) {
            candidates.extend_from_slice(bucket);
        }
        candidates.extend_from_slice(&self.universal);
        candidates.sort_unstable();
        candidates.dedup();
        candidates
    }

    /// Resuelve las declaraciones en cascada para `el` (UA → Author → Inline,
    /// respetando `!important` y especificidad).
    pub fn resolve_declarations<E: ElementView>(
        &self,
        el: &E,
        inline_style: &str,
    ) -> HashMap<String, Declaration> {
        struct Entry {
            important: bool,
            origin: Origin,
            specificity: Specificity,
            order: usize,
            decl: Declaration,
        }

        let mut matched: Vec<Entry> = Vec::new();
        for idx in self.candidate_indices(el) {
            let rule = &self.rules[idx];
            if selector_matches(el, &rule.selector) {
                for decl in &rule.declarations {
                    matched.push(Entry {
                        important: decl.important,
                        origin: rule.origin,
                        specificity: rule.specificity,
                        order: rule.order,
                        decl: decl.clone(),
                    });
                }
            }
        }

        for (i, decl) in parse_declarations(inline_style).into_iter().enumerate() {
            matched.push(Entry {
                important: decl.important,
                origin: Origin::Inline,
                specificity: Specificity {
                    ids: u16::MAX,
                    classes: 0,
                    tags: 0,
                },
                order: usize::MAX - 1024 + i,
                decl,
            });
        }

        // En `!important`, UserAgent `!important` > Author `!important` > Inline `!important`?
        // Para simplificar y coincidir con la cascada estándar CSS:
        // normal: UA < Author < Inline; !important: Inline < Author < UA (o peso normal invertido).
        matched.sort_by(|a, b| {
            let origin_rank = |entry: &Entry| -> u8 {
                if entry.important {
                    match entry.origin {
                        Origin::Inline => 0,
                        Origin::Author => 1,
                        Origin::UserAgent => 2,
                    }
                } else {
                    entry.origin as u8
                }
            };
            let key_a = (a.important, origin_rank(a), a.specificity, a.order);
            let key_b = (b.important, origin_rank(b), b.specificity, b.order);
            key_a.cmp(&key_b)
        });

        let mut resolved = HashMap::new();
        for entry in matched {
            resolved.insert(entry.decl.name.clone(), entry.decl);
        }
        resolved
    }
}

enum BucketKey {
    Id(String),
    Class(String),
    Tag(String),
    Universal,
}

fn rightmost_bucket_key(selector: &str) -> BucketKey {
    let rightmost = selector
        .split([' ', '>', '+', '~'])
        .rfind(|part| !part.trim().is_empty())
        .unwrap_or(selector)
        .trim();

    if let Some(pos) = rightmost.rfind('#') {
        let id: String = rightmost[pos + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !id.is_empty() {
            return BucketKey::Id(id);
        }
    }
    if let Some(pos) = rightmost.rfind('.') {
        let cls: String = rightmost[pos + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !cls.is_empty() {
            return BucketKey::Class(cls);
        }
    }
    let tag: String = rightmost
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if !tag.is_empty() {
        return BucketKey::Tag(tag.to_ascii_lowercase());
    }
    BucketKey::Universal
}

/// Comprueba si un selector (compuesto + combinadores ` ` y `>`) casa con `el`.
pub fn selector_matches<E: ElementView>(el: &E, selector: &str) -> bool {
    let tokens = tokenize_combinators(selector);
    if tokens.is_empty() {
        return false;
    }
    match_chain(el, &tokens, tokens.len() - 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    Descendant,
    Child,
}

enum SelectorStep<'a> {
    Compound(&'a str),
    Comb(Combinator),
}

fn tokenize_combinators(selector: &str) -> Vec<SelectorStep<'_>> {
    let mut steps = Vec::new();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;

    let bytes = selector.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' => {
                depth = (depth - 1).max(0);
                i += 1;
            }
            b'>' if depth == 0 => {
                let left = selector[start..i].trim();
                if !left.is_empty() {
                    parts.push(SelectorStep::Compound(left));
                }
                parts.push(SelectorStep::Comb(Combinator::Child));
                i += 1;
                start = i;
            }
            b' ' | b'\t' | b'\n' | b'\r' if depth == 0 => {
                let left = selector[start..i].trim();
                if !left.is_empty() {
                    parts.push(SelectorStep::Compound(left));
                    parts.push(SelectorStep::Comb(Combinator::Descendant));
                }
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    let tail = selector[start..].trim();
    if !tail.is_empty() {
        parts.push(SelectorStep::Compound(tail));
    }
    // Elimina combinadores sobrantes o duplicados (`a > b` genera Descendant + Child).
    for step in parts {
        match (&steps.last(), &step) {
            (Some(SelectorStep::Comb(_)), SelectorStep::Comb(Combinator::Child)) => {
                steps.pop();
                steps.push(step);
            }
            (Some(SelectorStep::Comb(_)), SelectorStep::Comb(Combinator::Descendant)) => {}
            _ => steps.push(step),
        }
    }
    while matches!(steps.last(), Some(SelectorStep::Comb(_))) {
        steps.pop();
    }
    steps
}

fn match_chain<E: ElementView>(el: &E, steps: &[SelectorStep<'_>], idx: usize) -> bool {
    let SelectorStep::Compound(compound) = steps[idx] else {
        return false;
    };
    if !match_compound(el, compound) {
        return false;
    }
    if idx == 0 {
        return true;
    }
    let Some(SelectorStep::Comb(comb)) = steps.get(idx - 1) else {
        return false;
    };
    let next_idx = idx.saturating_sub(2);
    match comb {
        Combinator::Child => el
            .parent()
            .is_some_and(|parent| match_chain(parent, steps, next_idx)),
        Combinator::Descendant => {
            let mut cur = el.parent();
            while let Some(ancestor) = cur {
                if match_chain(ancestor, steps, next_idx) {
                    return true;
                }
                cur = ancestor.parent();
            }
            false
        }
    }
}

fn match_compound<E: ElementView>(el: &E, compound: &str) -> bool {
    let bytes = compound.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'*' => {
                i += 1;
            }
            b'#' => {
                i += 1;
                let start = i;
                while i < bytes.len() && is_ident(bytes[i]) {
                    i += 1;
                }
                if el.id() != Some(&compound[start..i]) {
                    return false;
                }
            }
            b'.' => {
                i += 1;
                let start = i;
                while i < bytes.len() && is_ident(bytes[i]) {
                    i += 1;
                }
                let cls = &compound[start..i];
                let has_cls = el.has_class(cls)
                    || el
                        .attribute("class")
                        .is_some_and(|attr| attr.split_ascii_whitespace().any(|c| c == cls));
                if !has_cls {
                    return false;
                }
            }
            b'[' => {
                let Some(end_rel) = compound[i + 1..].find(']') else {
                    return false;
                };
                let inside = &compound[i + 1..i + 1 + end_rel];
                if !match_attribute(el, inside) {
                    return false;
                }
                i += end_rel + 2;
            }
            b':' => {
                // Pseudo-clases simples: avanzamos sobre el identificador.
                i += 1;
                while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'#' && bytes[i] != b'[' {
                    i += 1;
                }
            }
            _ if is_ident(bytes[i]) => {
                let start = i;
                while i < bytes.len() && is_ident(bytes[i]) {
                    i += 1;
                }
                if !el.tag_name().eq_ignore_ascii_case(&compound[start..i]) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn match_attribute<E: ElementView>(el: &E, inside: &str) -> bool {
    if let Some((attr, val)) = inside.split_once('=') {
        let attr = attr.trim();
        let val = val.trim().trim_matches('"').trim_matches('\'');
        el.attribute(attr).is_some_and(|actual| actual == val)
    } else {
        el.attribute(inside.trim()).is_some()
    }
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_lookup_avoids_scanning_unrelated_rules() {
        let mut css = String::new();
        for i in 0..800 {
            css.push_str(&format!(".unused-{i} {{ color: red; }}\n"));
        }
        css.push_str("div > p.target { color: #123456; }\n");

        let sheet = CssSheet::parse(&css, Origin::Author);
        let computer = StyleComputer::from_sheets(&[sheet], 1280.0, &["color", "display"]);
        assert!(computer.rule_count() > 800);

        let parent = SimpleElement {
            tag: "div".into(),
            ..Default::default()
        };
        let child = SimpleElement {
            tag: "p".into(),
            classes: vec!["target".into()],
            parent: Some(Box::new(parent)),
            ..Default::default()
        };

        let candidates = computer.candidate_indices(&child);
        let count = candidates.len();
        assert!(count < 20, "solo buckets del nodo ({count})");
        let decls = computer.resolve_declarations(&child, "");
        let color = decls.get("color").map(|d| d.value.as_str());
        assert_eq!(color, Some("#123456"));
    }

    #[test]
    fn cascade_respects_origin_specificity_and_important() {
        let sheet = CssSheet::parse(
            "p { color: blue !important; } #lead { color: green; }",
            Origin::Author,
        );
        let computer = StyleComputer::from_sheets(&[sheet], 1280.0, &["color"]);
        let el = SimpleElement {
            tag: "p".into(),
            id: Some("lead".into()),
            ..Default::default()
        };
        let decls = computer.resolve_declarations(&el, "color: red;");
        assert_eq!(decls.get("color").map(|d| d.value.as_str()), Some("blue"));
    }
}
