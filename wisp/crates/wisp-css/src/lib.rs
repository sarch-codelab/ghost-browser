//! Parser CSS propio, CSSOM mínimo, evaluador de `var()`/`calc()`/`min()`/
//! `max()`/`clamp()` y hoja de estilos de User-Agent (`default.css`) de Wisp.
//!
//! Cero dependencias externas: sustituye progresivamente al parser ad-hoc de
//! `wisp-engine` (ítems 9 y 10 del backlog).

#![forbid(unsafe_code)]

use std::collections::HashMap;

/// Hoja de estilos de User-Agent por defecto (ítem 9 del backlog).
pub const UA_STYLESHEET: &str = include_str!("../default.css");

/// Origen en la cascada CSS (orden creciente de prioridad normal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    UserAgent = 0,
    Author = 1,
    Inline = 2,
}

/// Especificidad CSS `(id, class_attr_pseudo, type_pseudo)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Specificity {
    pub ids: u16,
    pub classes: u16,
    pub tags: u16,
}

/// Declaración CSS individual (`propiedad: valor [!important]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub name: String,
    pub value: String,
    pub important: bool,
}

/// Regla de estilo (`selector { declaraciones }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CssRule {
    pub selector: String,
    pub specificity: Specificity,
    pub declarations: Vec<Declaration>,
    pub origin: Origin,
    pub order: usize,
}

/// Regla condicional `@media <query> { ... }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRule {
    pub condition: String,
    pub rules: Vec<CssStatement>,
}

/// Regla condicional `@supports <cond> { ... }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportsRule {
    pub condition: String,
    pub rules: Vec<CssStatement>,
}

/// Sentencia de nivel superior o anidada en un CSSOM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CssStatement {
    Rule(CssRule),
    Media(MediaRule),
    Supports(SupportsRule),
}

/// Hoja CSS parseada (CSSOM).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CssSheet {
    pub origin: Option<Origin>,
    pub statements: Vec<CssStatement>,
}

impl CssSheet {
    /// Parsea una hoja CSS con el origen indicado (`Origin::UserAgent` o `Origin::Author`).
    pub fn parse(css: &str, origin: Origin) -> Self {
        let cleaned = strip_css_comments(css);
        let mut order = 0usize;
        let statements = parse_statements(&cleaned, origin, &mut order);
        Self {
            origin: Some(origin),
            statements,
        }
    }

    /// Parsea la hoja de estilos de User-Agent integrada (`default.css`).
    pub fn user_agent() -> Self {
        Self::parse(UA_STYLESHEET, Origin::UserAgent)
    }

    /// Aplana las reglas aplicables para `viewport_width`, evaluando `@media`
    /// y `@supports` con la lista `supported_properties`.
    pub fn flatten_for_viewport(
        &self,
        viewport_width: f32,
        supported_properties: &[&str],
    ) -> Vec<CssRule> {
        let mut out = Vec::new();
        flatten_statements(
            &self.statements,
            viewport_width,
            supported_properties,
            &mut out,
        );
        out
    }
}

fn flatten_statements(
    statements: &[CssStatement],
    viewport_width: f32,
    supported_properties: &[&str],
    out: &mut Vec<CssRule>,
) {
    for stmt in statements {
        match stmt {
            CssStatement::Rule(rule) => out.push(rule.clone()),
            CssStatement::Media(media) => {
                if media_query_matches(&media.condition, viewport_width) {
                    flatten_statements(&media.rules, viewport_width, supported_properties, out);
                }
            }
            CssStatement::Supports(supports) => {
                if supports_query_matches(&supports.condition, supported_properties) {
                    flatten_statements(&supports.rules, viewport_width, supported_properties, out);
                }
            }
        }
    }
}

/// Elimina comentarios `/* ... */` respetando comillas.
pub fn strip_css_comments(css: &str) -> String {
    let bytes = css.as_bytes();
    let mut out = String::with_capacity(css.len());
    let mut i = 0usize;
    let mut quote: Option<u8> = None;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == b'\\' && i + 1 < bytes.len() {
                out.push_str(&css[i..i + 2]);
                i += 2;
                continue;
            }
            if b == q {
                quote = None;
            }
            let ch = css[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        if b == b'"' || b == b'\'' {
            quote = Some(b);
            out.push(b as char);
            i += 1;
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push(' ');
        } else {
            let ch = css[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn parse_statements(css: &str, origin: Origin, order: &mut usize) -> Vec<CssStatement> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut block_start = 0usize;
    let mut prelude = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in css.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(q) = quote {
            if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '{' => {
                if depth == 0 {
                    prelude = css[start..idx].trim().to_owned();
                    block_start = idx + ch.len_utf8();
                }
                depth += 1;
            }
            ';' if depth == 0 => {
                // Reglas `@import`, `@charset`, `@namespace` fuera de bloques.
                start = idx + 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let body = &css[block_start..idx];
                    if let Some(cond) = at_prelude(&prelude, "@media") {
                        out.push(CssStatement::Media(MediaRule {
                            condition: cond.to_owned(),
                            rules: parse_statements(body, origin, order),
                        }));
                    } else if let Some(cond) = at_prelude(&prelude, "@supports") {
                        out.push(CssStatement::Supports(SupportsRule {
                            condition: cond.to_owned(),
                            rules: parse_statements(body, origin, order),
                        }));
                    } else if !prelude.starts_with('@') {
                        let decls = parse_declarations(body);
                        if !decls.is_empty() {
                            for raw_sel in split_top_level(prelude.as_str(), ',') {
                                let sel = raw_sel.trim();
                                if sel.is_empty() {
                                    continue;
                                }
                                let normalized = if sel.eq_ignore_ascii_case(":root") {
                                    "html".to_owned()
                                } else {
                                    sel.to_owned()
                                };
                                out.push(CssStatement::Rule(CssRule {
                                    specificity: css_specificity(&normalized),
                                    selector: normalized,
                                    declarations: decls.clone(),
                                    origin,
                                    order: *order,
                                }));
                                *order += 1;
                            }
                        }
                    }
                    start = idx + ch.len_utf8();
                }
            }
            _ => {}
        }
    }
    out
}

fn at_prelude<'a>(prelude: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = prelude.trim();
    let head = trimmed.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = &trimmed[keyword.len()..];
    if rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('(') {
        Some(rest.trim())
    } else {
        None
    }
}

/// Parsea una lista de declaraciones `prop: val; ...` respetando cadenas y paréntesis.
pub fn parse_declarations(block: &str) -> Vec<Declaration> {
    let mut out = Vec::new();
    for part in split_top_level(block, ';') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((name, raw_val)) = trimmed.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let norm_name = if name.starts_with("--") {
            name.to_owned()
        } else {
            name.to_ascii_lowercase()
        };
        let (value, important) = strip_important(raw_val.trim());
        if value.is_empty() {
            continue;
        }
        out.push(Declaration {
            name: norm_name,
            value: value.to_owned(),
            important,
        });
    }
    out
}

fn strip_important(value: &str) -> (&str, bool) {
    let trimmed = value.trim_end();
    if trimmed.len() >= 10 {
        let tail = &trimmed[trimmed.len() - 9..];
        if tail.eq_ignore_ascii_case("important") {
            let before = trimmed[..trimmed.len() - 9].trim_end();
            if let Some(stripped) = before.strip_suffix('!') {
                return (stripped.trim_end(), true);
            }
        }
    }
    (trimmed, false)
}

/// Divide `text` por `sep` únicamente en el nivel superior de `()`/`[]` y comillas.
pub fn split_top_level(text: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0usize;

    for (idx, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(q) = quote {
            if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '(' => paren += 1,
            ')' => paren = (paren - 1).max(0),
            '[' => bracket += 1,
            ']' => bracket = (bracket - 1).max(0),
            c if c == sep && paren == 0 && bracket == 0 => {
                parts.push(&text[start..idx]);
                start = idx + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Calcula la especificidad aproximada de un selector simple o compuesto.
pub fn css_specificity(selector: &str) -> Specificity {
    let mut ids = 0u16;
    let mut classes = 0u16;
    let mut tags = 0u16;

    let bytes = selector.as_bytes();
    let mut i = 0usize;
    let mut in_attr = false;
    let mut quote: Option<u8> = None;

    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
            i += 1;
            continue;
        }
        if b == b'[' {
            in_attr = true;
            classes = classes.saturating_add(1);
            i += 1;
            continue;
        }
        if b == b']' {
            in_attr = false;
            i += 1;
            continue;
        }
        if in_attr {
            i += 1;
            continue;
        }
        match b {
            b'#' => {
                ids = ids.saturating_add(1);
                i += 1;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
            }
            b'.' => {
                classes = classes.saturating_add(1);
                i += 1;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
            }
            b':' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b':' {
                    tags = tags.saturating_add(1);
                    i += 2;
                } else {
                    classes = classes.saturating_add(1);
                    i += 1;
                }
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'(' {
                    // Pseudo-clase funcional: avanzamos hasta `)` si no es `:where`.
                    i += 1;
                }
            }
            b'*' | b'>' | b'+' | b'~' | b' ' | b'\t' | b'\n' | b'\r' | b'(' | b')' => {
                i += 1;
            }
            _ if is_ident_byte(b) => {
                tags = tags.saturating_add(1);
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    Specificity { ids, classes, tags }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Evalúa `@media` para un ancho de viewport dado.
pub fn media_query_matches(condition: &str, viewport_width: f32) -> bool {
    let trimmed = condition.trim();
    if trimmed.is_empty() {
        return true;
    }
    split_top_level(trimmed, ',')
        .iter()
        .any(|part| media_part_matches(part, viewport_width))
}

fn media_part_matches(query: &str, viewport_width: f32) -> bool {
    let lower = query.trim().to_ascii_lowercase();
    let (negated, rest) = if let Some(tail) = lower.strip_prefix("not ") {
        (true, tail.trim())
    } else if let Some(tail) = lower.strip_prefix("only ") {
        (false, tail.trim())
    } else {
        (false, lower.as_str())
    };

    let mut matched = true;
    for clause in split_top_level_ci(rest, " and ") {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        let ok = match clause {
            "all" | "screen" => true,
            "print" | "speech" => false,
            _ => {
                if let Some(inner) = clause.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
                    eval_media_feature(inner.trim(), viewport_width)
                } else {
                    true
                }
            }
        };
        if !ok {
            matched = false;
            break;
        }
    }
    if negated { !matched } else { matched }
}

fn eval_media_feature(feature: &str, viewport_width: f32) -> bool {
    let Some((name, raw_val)) = feature.split_once(':') else {
        return true;
    };
    let name = name.trim();
    let val = raw_val.trim();
    match name {
        "min-width" => parse_px_length(val).is_some_and(|px| viewport_width >= px),
        "max-width" => parse_px_length(val).is_some_and(|px| viewport_width <= px),
        "width" => parse_px_length(val).is_some_and(|px| (viewport_width - px).abs() < 0.5),
        "orientation" => {
            if val == "landscape" {
                viewport_width >= 800.0
            } else {
                viewport_width < 800.0
            }
        }
        "prefers-color-scheme" => val == "light",
        "prefers-reduced-motion" => val == "no-preference",
        _ => true,
    }
}

fn parse_px_length(raw: &str) -> Option<f32> {
    let s = raw.trim().to_ascii_lowercase();
    if let Some(n) = s.strip_suffix("px") {
        n.trim().parse::<f32>().ok()
    } else if let Some(n) = s.strip_suffix("rem").or_else(|| s.strip_suffix("em")) {
        n.trim().parse::<f32>().ok().map(|v| v * 16.0)
    } else if s == "0" {
        Some(0.0)
    } else {
        None
    }
}

/// Evalúa `@supports` contra la lista de propiedades implementadas.
pub fn supports_query_matches(condition: &str, supported_properties: &[&str]) -> bool {
    let trimmed = condition.trim();
    if trimmed.is_empty() {
        return false;
    }
    if let Some(rest) = trimmed
        .get(..4)
        .filter(|p| p.eq_ignore_ascii_case("not "))
        .map(|_| &trimmed[4..])
    {
        return !supports_query_matches(rest, supported_properties);
    }
    if let Some((left, right)) = split_once_top_level_ci(trimmed, " or ") {
        return supports_query_matches(left, supported_properties)
            || supports_query_matches(right, supported_properties);
    }
    if let Some((left, right)) = split_once_top_level_ci(trimmed, " and ") {
        return supports_query_matches(left, supported_properties)
            && supports_query_matches(right, supported_properties);
    }
    if let Some(inner) = trimmed.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        if let Some((prop, _val)) = inner.split_once(':') {
            let prop = prop.trim();
            if prop.starts_with("--") {
                return true;
            }
            return supported_properties
                .iter()
                .any(|p| p.eq_ignore_ascii_case(prop));
        }
        return supports_query_matches(inner, supported_properties);
    }
    if trimmed.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("selector")) {
        return trimmed.contains('(') && trimmed.ends_with(')');
    }
    false
}

fn split_once_top_level_ci<'a>(value: &'a str, needle: &str) -> Option<(&'a str, &'a str)> {
    let bytes = value.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut depth = 0i32;
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
            _ => {
                if depth == 0
                    && i + needle_bytes.len() <= bytes.len()
                    && bytes[i..i + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes)
                {
                    return Some((
                        value[..i].trim(),
                        value[i + needle_bytes.len()..].trim(),
                    ));
                }
                i += 1;
            }
        }
    }
    None
}

fn split_top_level_ci<'a>(value: &'a str, needle: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut rest = value;
    while let Some((left, right)) = split_once_top_level_ci(rest, needle) {
        parts.push(left);
        rest = right;
    }
    parts.push(rest);
    parts
}

/// Resuelve `var(--nombre, fallback)` de forma recursiva con límite de profundidad.
pub fn resolve_var_references(value: &str, custom_props: &HashMap<String, String>) -> String {
    resolve_var_depth(value, custom_props, 0)
}

fn resolve_var_depth(value: &str, custom_props: &HashMap<String, String>, depth: usize) -> String {
    if depth >= 16 || !value.contains("var(") {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let mut idx = 0usize;
    while let Some(pos) = value[idx..].find("var(") {
        let abs = idx + pos;
        out.push_str(&value[idx..abs]);
        let args_start = abs + 4;
        if let Some(close_rel) = find_matching_paren(&value[args_start..]) {
            let inner = &value[args_start..args_start + close_rel];
            let args = split_top_level(inner, ',');
            let var_name = args.first().map(|s| s.trim()).unwrap_or("");
            if let Some(resolved) = custom_props.get(var_name) {
                out.push_str(&resolve_var_depth(resolved, custom_props, depth + 1));
            } else if args.len() > 1 {
                let fallback = args[1..].join(",");
                out.push_str(&resolve_var_depth(fallback.trim(), custom_props, depth + 1));
            }
            idx = args_start + close_rel + 1;
        } else {
            out.push_str(&value[abs..]);
            return out;
        }
    }
    out.push_str(&value[idx..]);
    out
}

fn find_matching_paren(rest: &str) -> Option<usize> {
    let mut depth = 1i32;
    for (idx, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

/// Evalúa expresiones matemáticas CSS `calc(...)`, `min(...)`, `max(...)` y
/// `clamp(min, val, max)` devolviendo el valor en píxeles (con `font_size` y
/// `container_px` opcional para `%`).
pub fn eval_math_px(expr: &str, font_size: f32, container_px: Option<f32>) -> Option<f32> {
    let trimmed = expr.trim();
    if let Some(inner) = strip_fn(trimmed, "calc") {
        return eval_calc_expr(inner, font_size, container_px);
    }
    if let Some(inner) = strip_fn(trimmed, "min") {
        let mut best: Option<f32> = None;
        for arg in split_top_level(inner, ',') {
            let v = eval_calc_expr(arg, font_size, container_px)?;
            best = Some(best.map_or(v, |b| b.min(v)));
        }
        return best;
    }
    if let Some(inner) = strip_fn(trimmed, "max") {
        let mut best: Option<f32> = None;
        for arg in split_top_level(inner, ',') {
            let v = eval_calc_expr(arg, font_size, container_px)?;
            best = Some(best.map_or(v, |b| b.max(v)));
        }
        return best;
    }
    if let Some(inner) = strip_fn(trimmed, "clamp") {
        let args = split_top_level(inner, ',');
        if args.len() != 3 {
            return None;
        }
        let min_v = eval_calc_expr(args[0], font_size, container_px)?;
        let val = eval_calc_expr(args[1], font_size, container_px)?;
        let max_v = eval_calc_expr(args[2], font_size, container_px)?;
        return Some(val.max(min_v).min(max_v));
    }
    parse_unit_px(trimmed, font_size, container_px)
}

fn strip_fn<'a>(expr: &'a str, name: &str) -> Option<&'a str> {
    let trimmed = expr.trim();
    let head = trimmed.get(..name.len())?;
    if !head.eq_ignore_ascii_case(name) {
        return None;
    }
    let after = trimmed[name.len()..].strip_prefix('(')?;
    after.strip_suffix(')')
}

fn eval_calc_expr(expr: &str, font_size: f32, container_px: Option<f32>) -> Option<f32> {
    let tokens = tokenize_calc(expr)?;
    let mut pos = 0usize;
    let val = parse_add_sub(&tokens, &mut pos, font_size, container_px)?;
    if pos == tokens.len() {
        Some(val)
    } else {
        None
    }
}

#[derive(Debug, Clone)]
enum CalcTok {
    Op(char),
    LParen,
    RParen,
    Atom(String),
}

fn tokenize_calc(expr: &str) -> Option<Vec<CalcTok>> {
    let mut toks = Vec::new();
    let bytes = expr.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match b {
            b'(' => {
                toks.push(CalcTok::LParen);
                i += 1;
            }
            b')' => {
                toks.push(CalcTok::RParen);
                i += 1;
            }
            b'+' | b'*' | b'/' => {
                toks.push(CalcTok::Op(b as char));
                i += 1;
            }
            b'-' => {
                // `-` unario si está al inicio o tras operador / `(`.
                let is_unary = toks.is_empty()
                    || matches!(toks.last(), Some(CalcTok::Op(_) | CalcTok::LParen));
                if is_unary {
                    let start = i;
                    i += 1;
                    while i < bytes.len()
                        && !bytes[i].is_ascii_whitespace()
                        && !matches!(bytes[i], b'+' | b'*' | b'/' | b'(' | b')')
                    {
                        i += 1;
                    }
                    toks.push(CalcTok::Atom(expr[start..i].to_owned()));
                } else {
                    toks.push(CalcTok::Op('-'));
                    i += 1;
                }
            }
            _ => {
                let start = i;
                let mut depth = 0i32;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c == b'(' {
                        depth += 1;
                    } else if c == b')' {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    } else if depth == 0
                        && (c.is_ascii_whitespace() || matches!(c, b'+' | b'-' | b'*' | b'/'))
                    {
                        break;
                    }
                    i += 1;
                }
                toks.push(CalcTok::Atom(expr[start..i].to_owned()));
            }
        }
    }
    Some(toks)
}

fn parse_add_sub(
    toks: &[CalcTok],
    pos: &mut usize,
    font_size: f32,
    container_px: Option<f32>,
) -> Option<f32> {
    let mut lhs = parse_mul_div(toks, pos, font_size, container_px)?;
    while let Some(CalcTok::Op(op @ ('+' | '-'))) = toks.get(*pos) {
        *pos += 1;
        let rhs = parse_mul_div(toks, pos, font_size, container_px)?;
        lhs = if *op == '+' { lhs + rhs } else { lhs - rhs };
    }
    Some(lhs)
}

fn parse_mul_div(
    toks: &[CalcTok],
    pos: &mut usize,
    font_size: f32,
    container_px: Option<f32>,
) -> Option<f32> {
    let mut lhs = parse_primary(toks, pos, font_size, container_px)?;
    while let Some(CalcTok::Op(op @ ('*' | '/'))) = toks.get(*pos) {
        *pos += 1;
        let rhs = parse_primary(toks, pos, font_size, container_px)?;
        if *op == '*' {
            lhs *= rhs;
        } else {
            if rhs == 0.0 {
                return None;
            }
            lhs /= rhs;
        }
    }
    Some(lhs)
}

fn parse_primary(
    toks: &[CalcTok],
    pos: &mut usize,
    font_size: f32,
    container_px: Option<f32>,
) -> Option<f32> {
    match toks.get(*pos)? {
        CalcTok::LParen => {
            *pos += 1;
            let val = parse_add_sub(toks, pos, font_size, container_px)?;
            if matches!(toks.get(*pos), Some(CalcTok::RParen)) {
                *pos += 1;
                Some(val)
            } else {
                None
            }
        }
        CalcTok::Atom(atom) => {
            *pos += 1;
            eval_math_px(atom, font_size, container_px)
        }
        _ => None,
    }
}

fn parse_unit_px(raw: &str, font_size: f32, container_px: Option<f32>) -> Option<f32> {
    let s = raw.trim().to_ascii_lowercase();
    if let Some(n) = s.strip_suffix("px") {
        n.trim().parse::<f32>().ok()
    } else if let Some(n) = s.strip_suffix("rem") {
        n.trim().parse::<f32>().ok().map(|v| v * 16.0)
    } else if let Some(n) = s.strip_suffix("em") {
        n.trim().parse::<f32>().ok().map(|v| v * font_size)
    } else if let Some(n) = s.strip_suffix('%') {
        let pct = n.trim().parse::<f32>().ok()?;
        container_px.map(|c| c * (pct / 100.0))
    } else {
        s.parse::<f32>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ua_stylesheet() {
        let ua = CssSheet::user_agent();
        let rules = ua.flatten_for_viewport(1280.0, &["display", "color", "font-size"]);
        assert!(rules.iter().any(|r| r.selector == "h1"));
        assert!(rules.iter().any(|r| r.selector == "a"));
    }

    #[test]
    fn supports_media_and_supports_rules() {
        let css = r#"
            @media (min-width: 800px) {
                .card { display: flex; }
            }
            @media (max-width: 400px) {
                .mobile { display: block; }
            }
            @supports (display: grid) and (not (foo-prop: 1)) {
                .grid { display: grid; }
            }
        "#;
        let sheet = CssSheet::parse(css, Origin::Author);
        let rules = sheet.flatten_for_viewport(1280.0, &["display"]);
        let selectors: Vec<&str> = rules.iter().map(|r| r.selector.as_str()).collect();
        assert_eq!(selectors, vec![".card", ".grid"]);
    }

    #[test]
    fn resolves_vars_and_math_functions() {
        let mut vars = HashMap::new();
        vars.insert("--pad".to_owned(), "12px".to_owned());
        let resolved = resolve_var_references("calc(var(--pad) + var(--missing, 8px))", &vars);
        assert_eq!(resolved, "calc(12px + 8px)");
        assert_eq!(eval_math_px(&resolved, 16.0, None), Some(20.0));
        assert_eq!(
            eval_math_px("clamp(10px, 50%, 100px)", 16.0, Some(300.0)),
            Some(100.0)
        );
        assert_eq!(eval_math_px("min(2em, 20px)", 16.0, None), Some(20.0));
        assert_eq!(eval_math_px("max(1rem, 24px)", 16.0, None), Some(24.0));
    }

    #[test]
    fn computes_specificity_correctly() {
        assert!(css_specificity("#nav .item a") > css_specificity(".item a"));
        assert!(css_specificity(".item a") > css_specificity("div a"));
        assert_eq!(
            css_specificity("ul#list li.active"),
            Specificity {
                ids: 1,
                classes: 1,
                tags: 2
            }
        );
    }
}
