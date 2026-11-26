//! Script de build de `wisp-idl`.
//!
//! Lee los fragmentos WebIDL de `webidl/` y escribe los descriptores de
//! interfaz en Rust a `$OUT_DIR/descriptors.rs`, que `src/lib.rs` incluye con
//! `include!`. El parser no tiene dependencias y cubre solo el subconjunto de
//! WebIDL que Wisp usa hoy: interfaces con herencia simple, atributos
//! extendidos sobre la interfaz y sus miembros, declaraciones `const`,
//! atributos (`readonly`, `[SameObject]`, `[CEReactions]`, `[Replaceable]`,
//! `[LegacyUnforgeable]`), operaciones y argumentos (opcionales, variádicos,
//! anulables, uniones y valores por defecto).

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let webidl_dir = manifest.join("webidl");
    println!("cargo:rerun-if-changed={}", webidl_dir.display());

    let mut files: Vec<PathBuf> = fs::read_dir(&webidl_dir)
        .unwrap_or_else(|e| panic!("no se puede leer {}: {e}", webidl_dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("webidl"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no hay fragmentos .webidl en {}",
        webidl_dir.display()
    );

    let mut interfaces = Vec::new();
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let src = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("no se puede leer {}: {e}", file.display()));
        let parsed = parse_document(&src).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        interfaces.extend(parsed);
    }

    interfaces.sort_by(|a, b| a.name.cmp(&b.name));
    for pair in interfaces.windows(2) {
        assert_ne!(
            pair[0].name, pair[1].name,
            "interfaz duplicada `{}`",
            pair[1].name
        );
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let dest = Path::new(&out_dir).join("descriptors.rs");
    fs::write(&dest, generate(&interfaces))
        .unwrap_or_else(|e| panic!("no se puede escribir {}: {e}", dest.display()));
}

// ---------------------------------------------------------------------------
// Modelo intermedio
// ---------------------------------------------------------------------------

struct InterfaceDef {
    name: String,
    parent: Option<String>,
    exposed: Vec<String>,
    global: Option<String>,
    constructible: bool,
    constants: Vec<ConstantDef>,
    attributes: Vec<AttributeDef>,
    operations: Vec<OperationDef>,
}

struct ConstantDef {
    name: String,
    type_: String,
    value: String,
}

struct AttributeDef {
    name: String,
    type_: String,
    nullable: bool,
    readonly: bool,
    same_object: bool,
    replaceable: bool,
    legacy_unforgeable: bool,
    cereactions: bool,
}

struct OperationDef {
    name: String,
    return_type: String,
    return_nullable: bool,
    cereactions: bool,
    arguments: Vec<ArgumentDef>,
}

struct ArgumentDef {
    name: String,
    type_: String,
    optional: bool,
    variadic: bool,
    nullable: bool,
    default: Option<String>,
}

struct ExtendedAttr {
    name: String,
    value: Option<String>,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn new(src: &str) -> Self {
        Self {
            chars: src.chars().collect(),
            pos: 0,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn consume_char(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_char(&mut self, c: char) -> Result<(), String> {
        if self.consume_char(c) {
            Ok(())
        } else {
            Err(format!(
                "se esperaba `{c}` en la posición {}, se encontró {:?}",
                self.pos,
                self.peek()
            ))
        }
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        self.skip_ws();
        let kw: Vec<char> = keyword.chars().collect();
        let end = self.pos + kw.len();
        if end > self.chars.len() || self.chars[self.pos..end] != kw[..] {
            return false;
        }
        if let Some(next) = self.chars.get(end)
            && (next.is_alphanumeric() || *next == '_')
        {
            return false;
        }
        self.pos = end;
        true
    }

    fn parse_identifier(&mut self) -> Result<String, String> {
        self.skip_ws();
        let mut ident = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                ident.push(c);
                self.pos += 1;
            } else {
                break;
            }
        }
        if ident.is_empty() {
            Err(format!(
                "se esperaba un identificador en la posición {}",
                self.pos
            ))
        } else {
            Ok(ident)
        }
    }
}

fn strip_comments(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn parse_document(src: &str) -> Result<Vec<InterfaceDef>, String> {
    let cleaned = strip_comments(src);
    let mut p = Parser::new(&cleaned);
    let mut out = Vec::new();

    loop {
        p.skip_ws();
        if p.eof() {
            break;
        }

        let iface_attrs = if p.peek() == Some('[') {
            parse_extended_attributes(&mut p)?
        } else {
            Vec::new()
        };

        if !p.consume_keyword("interface") {
            return Err(format!("se esperaba `interface` en la posición {}", p.pos));
        }

        let name = p.parse_identifier()?;
        p.skip_ws();
        let parent = if p.consume_char(':') {
            Some(p.parse_identifier()?)
        } else {
            None
        };
        p.skip_ws();
        p.expect_char('{')?;

        let mut def = InterfaceDef {
            name,
            parent,
            exposed: Vec::new(),
            global: None,
            constructible: false,
            constants: Vec::new(),
            attributes: Vec::new(),
            operations: Vec::new(),
        };

        for attr in &iface_attrs {
            match attr.name.as_str() {
                "Exposed" => {
                    if let Some(value) = &attr.value {
                        def.exposed = value.split('|').map(|s| s.trim().to_string()).collect();
                    }
                }
                "Global" => def.global = attr.value.clone(),
                _ => {}
            }
        }

        loop {
            p.skip_ws();
            match p.peek() {
                None => return Err(format!("interfaz `{}` sin cerrar", def.name)),
                Some('}') => {
                    p.bump();
                    break;
                }
                _ => {}
            }
            let (member, attrs) = read_member(&mut p)?;
            if !member.is_empty() {
                classify_member(&mut def, &member, &attrs)?;
            }
        }

        p.skip_ws();
        p.consume_char(';');
        out.push(def);
    }

    Ok(out)
}

fn read_member(p: &mut Parser) -> Result<(String, Vec<ExtendedAttr>), String> {
    p.skip_ws();
    let attrs = if p.peek() == Some('[') {
        parse_extended_attributes(p)?
    } else {
        Vec::new()
    };
    p.skip_ws();

    let mut buf = String::new();
    let mut depth = 0i32;
    loop {
        match p.peek() {
            None => return Err("miembro sin terminar".to_string()),
            Some(';') if depth == 0 => {
                p.bump();
                break;
            }
            Some('}') if depth == 0 => break,
            Some(c) => {
                match c {
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth -= 1,
                    _ => {}
                }
                buf.push(c);
                p.bump();
            }
        }
    }

    Ok((buf.trim().to_string(), attrs))
}

fn parse_extended_attributes(p: &mut Parser) -> Result<Vec<ExtendedAttr>, String> {
    p.expect_char('[')?;
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut depth = 0i32;
    loop {
        match p.bump() {
            None => return Err("lista de atributos extendidos sin cerrar".to_string()),
            Some(']') if depth == 0 => {
                push_attr(&mut out, &buf);
                break;
            }
            Some('(') => {
                depth += 1;
                buf.push('(');
            }
            Some(')') => {
                depth -= 1;
                buf.push(')');
            }
            Some(',') if depth == 0 => {
                push_attr(&mut out, &buf);
                buf.clear();
            }
            Some(c) => buf.push(c),
        }
    }
    Ok(out)
}

fn push_attr(out: &mut Vec<ExtendedAttr>, buf: &str) {
    let text = buf.trim();
    if text.is_empty() {
        return;
    }
    out.push(match text.split_once('=') {
        Some((name, value)) => ExtendedAttr {
            name: name.trim().to_string(),
            value: Some(value.trim().to_string()),
        },
        None => ExtendedAttr {
            name: text.to_string(),
            value: None,
        },
    });
}

fn has_attr(attrs: &[ExtendedAttr], name: &str) -> bool {
    attrs.iter().any(|a| a.name == name)
}

fn classify_member(
    def: &mut InterfaceDef,
    member: &str,
    attrs: &[ExtendedAttr],
) -> Result<(), String> {
    if let Some(rest) = member.strip_prefix("const ") {
        def.constants.push(parse_constant(rest)?);
    } else if member.starts_with("constructor") {
        def.constructible = true;
    } else if member.contains('(') {
        def.operations.push(parse_operation(member, attrs)?);
    } else if member.contains("attribute") {
        def.attributes.push(parse_attribute(member, attrs)?);
    } else {
        return Err(format!(
            "miembro no soportado en `{}`: `{}`",
            def.name, member
        ));
    }
    Ok(())
}

fn parse_constant(rest: &str) -> Result<ConstantDef, String> {
    let (left, value) = rest
        .split_once('=')
        .ok_or_else(|| format!("constante sin valor: `{rest}`"))?;
    let (type_, name, _) = split_type_name(left)?;
    Ok(ConstantDef {
        name,
        type_,
        value: value.trim().to_string(),
    })
}

fn parse_attribute(member: &str, attrs: &[ExtendedAttr]) -> Result<AttributeDef, String> {
    let mut rest = member.trim();
    let readonly = if let Some(tail) = rest.strip_prefix("readonly ") {
        rest = tail.trim();
        true
    } else {
        false
    };
    let body = rest
        .strip_prefix("attribute ")
        .ok_or_else(|| format!("atributo mal formado: `{member}`"))?
        .trim();
    let (type_, name, nullable) = split_type_name(body)?;
    Ok(AttributeDef {
        name,
        type_,
        nullable,
        readonly,
        same_object: has_attr(attrs, "SameObject"),
        replaceable: has_attr(attrs, "Replaceable"),
        legacy_unforgeable: has_attr(attrs, "LegacyUnforgeable"),
        cereactions: has_attr(attrs, "CEReactions"),
    })
}

fn parse_operation(member: &str, attrs: &[ExtendedAttr]) -> Result<OperationDef, String> {
    let open = member
        .find('(')
        .ok_or_else(|| format!("operación sin `(`: `{member}`"))?;
    let close = member
        .rfind(')')
        .ok_or_else(|| format!("operación sin `)`: `{member}`"))?;
    let before = member[..open].trim();
    let args_src = &member[open + 1..close];

    let (return_type, name, return_nullable) = split_type_name(before)?;

    let arguments = if args_src.trim().is_empty() {
        Vec::new()
    } else {
        split_top_level_commas(args_src)
            .iter()
            .map(|a| parse_argument(a))
            .collect::<Result<Vec<_>, _>>()?
    };

    Ok(OperationDef {
        name,
        return_type,
        return_nullable,
        cereactions: has_attr(attrs, "CEReactions"),
        arguments,
    })
}

fn parse_argument(raw: &str) -> Result<ArgumentDef, String> {
    let mut text = raw.trim();
    let optional = if let Some(tail) = text.strip_prefix("optional ") {
        text = tail.trim();
        true
    } else {
        false
    };

    let (left, default) = match text.split_once('=') {
        Some((l, r)) => (l.trim().to_string(), Some(r.trim().to_string())),
        None => (text.to_string(), None),
    };

    let (type_, name, nullable) = split_type_name(&left)?;
    let mut type_ = type_;
    let variadic = type_.ends_with("...");
    if variadic {
        type_ = type_.trim_end_matches('.').trim().to_string();
    }

    Ok(ArgumentDef {
        name,
        type_,
        optional,
        variadic,
        nullable,
        default,
    })
}

/// Divide `TYPE NAME` en tipo y nombre, informando además si el tipo es
/// anulable (`TYPE?`). El nombre es la última palabra; el tipo es todo lo
/// anterior, lo que soporta tipos con espacios (`unsigned short`) y uniones
/// entre paréntesis (`(A or B)`).
fn split_type_name(text: &str) -> Result<(String, String, bool), String> {
    let text = text.trim();
    let split = text
        .rfind(char::is_whitespace)
        .ok_or_else(|| format!("no se puede separar tipo y nombre en `{text}`"))?;
    let (type_part, name) = text.split_at(split);
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("nombre vacío en `{text}`"));
    }
    let type_part = type_part.trim();
    let (type_, nullable) = match type_part.strip_suffix('?') {
        Some(rest) => (rest.trim().to_string(), true),
        None => (type_part.to_string(), false),
    };
    Ok((type_, name.to_string(), nullable))
}

fn split_top_level_commas(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '(' | '[' => {
                depth += 1;
                buf.push(c);
            }
            ')' | ']' => {
                depth -= 1;
                buf.push(c);
            }
            ',' if depth == 0 => {
                out.push(buf.trim().to_string());
                buf.clear();
            }
            _ => buf.push(c),
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf.trim().to_string());
    }
    out
}

// ---------------------------------------------------------------------------
// Codegen
// ---------------------------------------------------------------------------

fn str_lit(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

fn opt_str_lit(value: Option<&str>) -> String {
    match value {
        Some(text) => format!("Some({})", str_lit(text)),
        None => "None".to_string(),
    }
}

fn bool_lit(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn generate(interfaces: &[InterfaceDef]) -> String {
    let mut out = String::new();
    writeln!(out, "// @generado por wisp-idl/build.rs; no editar a mano.").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "pub static INTERFACES: &[Interface] = &[").unwrap();

    for iface in interfaces {
        writeln!(out, "    Interface {{").unwrap();
        writeln!(out, "        name: {},", str_lit(&iface.name)).unwrap();
        writeln!(
            out,
            "        parent: {},",
            opt_str_lit(iface.parent.as_deref())
        )
        .unwrap();
        let exposed = iface
            .exposed
            .iter()
            .map(|e| str_lit(e))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "        exposed: &[{exposed}],").unwrap();
        writeln!(
            out,
            "        global: {},",
            opt_str_lit(iface.global.as_deref())
        )
        .unwrap();
        writeln!(
            out,
            "        constructible: {},",
            bool_lit(iface.constructible)
        )
        .unwrap();

        writeln!(out, "        constants: &[").unwrap();
        for c in &iface.constants {
            writeln!(
                out,
                "            Constant {{ name: {}, type_: {}, value: {} }},",
                str_lit(&c.name),
                str_lit(&c.type_),
                str_lit(&c.value)
            )
            .unwrap();
        }
        writeln!(out, "        ],").unwrap();

        writeln!(out, "        attributes: &[").unwrap();
        for a in &iface.attributes {
            writeln!(
                out,
                "            Attribute {{ name: {}, type_: {}, nullable: {}, readonly: {}, same_object: {}, replaceable: {}, legacy_unforgeable: {}, cereactions: {} }},",
                str_lit(&a.name),
                str_lit(&a.type_),
                bool_lit(a.nullable),
                bool_lit(a.readonly),
                bool_lit(a.same_object),
                bool_lit(a.replaceable),
                bool_lit(a.legacy_unforgeable),
                bool_lit(a.cereactions),
            )
            .unwrap();
        }
        writeln!(out, "        ],").unwrap();

        writeln!(out, "        operations: &[").unwrap();
        for o in &iface.operations {
            writeln!(out, "            Operation {{").unwrap();
            writeln!(out, "                name: {},", str_lit(&o.name)).unwrap();
            writeln!(
                out,
                "                return_type: {},",
                str_lit(&o.return_type)
            )
            .unwrap();
            writeln!(
                out,
                "                return_nullable: {},",
                bool_lit(o.return_nullable)
            )
            .unwrap();
            writeln!(
                out,
                "                cereactions: {},",
                bool_lit(o.cereactions)
            )
            .unwrap();
            writeln!(out, "                arguments: &[").unwrap();
            for arg in &o.arguments {
                writeln!(
                    out,
                    "                    Argument {{ name: {}, type_: {}, optional: {}, variadic: {}, nullable: {}, default: {} }},",
                    str_lit(&arg.name),
                    str_lit(&arg.type_),
                    bool_lit(arg.optional),
                    bool_lit(arg.variadic),
                    bool_lit(arg.nullable),
                    opt_str_lit(arg.default.as_deref()),
                )
                .unwrap();
            }
            writeln!(out, "                ],").unwrap();
            writeln!(out, "            }},").unwrap();
        }
        writeln!(out, "        ],").unwrap();

        writeln!(out, "    }},").unwrap();
    }

    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();
    let js_bindings = generate_js_bindings(interfaces);
    writeln!(
        out,
        "pub static JS_IDL_BINDINGS: &str = {};",
        str_lit(&js_bindings)
    )
    .unwrap();
    out
}

fn generate_js_bindings(interfaces: &[InterfaceDef]) -> String {
    let mut js = String::new();
    writeln!(js, "// @generado por wisp-idl/build.rs").unwrap();
    writeln!(js, "(function (g) {{").unwrap();
    writeln!(js, "  \"use strict\";").unwrap();
    writeln!(js, "  function link(child, parent) {{").unwrap();
    writeln!(
        js,
        "    if (typeof g[child] === \"function\" && typeof g[parent] === \"function\") {{"
    )
    .unwrap();
    writeln!(
        js,
        "      Object.setPrototypeOf(g[child].prototype, g[parent].prototype);"
    )
    .unwrap();
    writeln!(js, "    }}").unwrap();
    writeln!(js, "  }}").unwrap();

    for iface in interfaces {
        if let Some(parent) = &iface.parent {
            writeln!(js, "  link(\"{}\", \"{}\");", iface.name, parent).unwrap();
        }
        if !iface.constants.is_empty() {
            writeln!(js, "  if (typeof g[\"{}\"] === \"function\") {{", iface.name).unwrap();
            for c in &iface.constants {
                writeln!(
                    js,
                    "    Object.defineProperty(g[\"{}\"], \"{}\", {{ value: {}, enumerable: true }});",
                    iface.name, c.name, c.value
                )
                .unwrap();
                writeln!(
                    js,
                    "    Object.defineProperty(g[\"{}\"].prototype, \"{}\", {{ value: {}, enumerable: true }});",
                    iface.name, c.name, c.value
                )
                .unwrap();
            }
            writeln!(js, "  }}").unwrap();
        }
    }
    writeln!(js, "}})(globalThis);").unwrap();
    js
}
