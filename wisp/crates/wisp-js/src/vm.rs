//! Motor JavaScript propio de Wisp (ítem 15 del backlog):
//! `lexer -> parser -> AST -> bytecode -> intérprete` (VM de pila).
//!
//! Independiente de Boa y V8: permite ejecutar expresiones, variables,
//! control de flujo, funciones, cierres, objetos, excepciones, arrays y
//! bucles con presupuesto determinista de instrucciones y recursión, más
//! puente extensible (`HostBridge`) para bindings DOM nativos en Rust.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;

const MAX_VM_STEPS: usize = 200_000;
const MAX_RECURSION_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub enum JsVal {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<JsVal>),
    Object(usize),
    DomNode(usize),
    Function(usize),
    NativeHandle(String),
}

impl JsVal {
    pub fn is_truthy(&self) -> bool {
        match self {
            JsVal::Undefined | JsVal::Null => false,
            JsVal::Bool(b) => *b,
            JsVal::Number(n) => *n != 0.0 && !n.is_nan(),
            JsVal::Str(s) => !s.is_empty(),
            JsVal::Array(_)
            | JsVal::Object(_)
            | JsVal::DomNode(_)
            | JsVal::Function(_)
            | JsVal::NativeHandle(_) => true,
        }
    }

    pub fn to_number(&self) -> f64 {
        match self {
            JsVal::Undefined => f64::NAN,
            JsVal::Null | JsVal::Bool(false) => 0.0,
            JsVal::Bool(true) => 1.0,
            JsVal::Number(n) => *n,
            JsVal::Str(s) => {
                let t = s.trim();
                if t.is_empty() {
                    0.0
                } else {
                    t.parse::<f64>().unwrap_or(f64::NAN)
                }
            }
            JsVal::Array(_)
            | JsVal::Object(_)
            | JsVal::DomNode(_)
            | JsVal::Function(_)
            | JsVal::NativeHandle(_) => f64::NAN,
        }
    }
}

impl fmt::Display for JsVal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JsVal::Undefined => write!(f, "undefined"),
            JsVal::Null => write!(f, "null"),
            JsVal::Bool(b) => write!(f, "{b}"),
            JsVal::Number(n) => {
                if n.is_nan() {
                    write!(f, "NaN")
                } else if n.is_infinite() {
                    if *n > 0.0 {
                        write!(f, "Infinity")
                    } else {
                        write!(f, "-Infinity")
                    }
                } else if n.fract() == 0.0 && n.abs() < 1e15 {
                    write!(f, "{}", *n as i64)
                } else {
                    write!(f, "{n}")
                }
            }
            JsVal::Str(s) => write!(f, "{s}"),
            JsVal::Array(items) => {
                let parts: Vec<String> = items.iter().map(ToString::to_string).collect();
                write!(f, "{}", parts.join(","))
            }
            JsVal::Object(_) => write!(f, "[object Object]"),
            JsVal::DomNode(id) => write!(f, "[DomNode #{id}]"),
            JsVal::Function(idx) => write!(f, "[Function #{idx}]"),
            JsVal::NativeHandle(name) => write!(f, "[{name}]"),
        }
    }
}

/// Puente hacia objetos y APIs del host (DOM, `fetch`, `URL`, `console`, etc.).
pub trait HostBridge {
    fn get_prop(&mut self, vm: &mut VmRuntime, target: &JsVal, prop: &str) -> Result<JsVal, JsVal>;

    fn set_prop(
        &mut self,
        vm: &mut VmRuntime,
        target: &JsVal,
        prop: &str,
        value: JsVal,
    ) -> Result<(), JsVal>;

    fn call_method(
        &mut self,
        vm: &mut VmRuntime,
        target: &JsVal,
        method: &str,
        args: Vec<JsVal>,
    ) -> Result<JsVal, JsVal>;

    fn call_global(
        &mut self,
        vm: &mut VmRuntime,
        name: &str,
        args: Vec<JsVal>,
    ) -> Option<Result<JsVal, JsVal>>;

    fn construct(
        &mut self,
        vm: &mut VmRuntime,
        ctor: &str,
        args: Vec<JsVal>,
    ) -> Result<JsVal, JsVal>;
}

struct NoopHost;

impl HostBridge for NoopHost {
    fn get_prop(
        &mut self,
        _vm: &mut VmRuntime,
        _target: &JsVal,
        _prop: &str,
    ) -> Result<JsVal, JsVal> {
        Ok(JsVal::Undefined)
    }

    fn set_prop(
        &mut self,
        _vm: &mut VmRuntime,
        _target: &JsVal,
        _prop: &str,
        _value: JsVal,
    ) -> Result<(), JsVal> {
        Ok(())
    }

    fn call_method(
        &mut self,
        _vm: &mut VmRuntime,
        _target: &JsVal,
        method: &str,
        _args: Vec<JsVal>,
    ) -> Result<JsVal, JsVal> {
        Err(JsVal::Str(format!("método no definido `{method}`")))
    }

    fn call_global(
        &mut self,
        _vm: &mut VmRuntime,
        _name: &str,
        _args: Vec<JsVal>,
    ) -> Option<Result<JsVal, JsVal>> {
        None
    }

    fn construct(
        &mut self,
        _vm: &mut VmRuntime,
        ctor: &str,
        _args: Vec<JsVal>,
    ) -> Result<JsVal, JsVal> {
        Err(JsVal::Str(format!("constructor no definido `{ctor}`")))
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    KwLet,
    KwConst,
    KwVar,
    KwFunction,
    KwClass,
    KwExtends,
    KwThis,
    KwSuper,
    KwOf,
    KwIn,
    KwReturn,
    KwIf,
    KwElse,
    KwWhile,
    KwFor,
    KwSwitch,
    KwCase,
    KwDefault,
    KwBreak,
    KwContinue,
    KwTry,
    KwCatch,
    KwFinally,
    KwThrow,
    KwNew,
    KwTypeof,
    KwVoid,
    KwAsync,
    KwAwait,
    KwTrue,
    KwFalse,
    KwNull,
    KwUndefined,
    Template(String),
    Regex(String, String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusPlus,
    MinusMinus,
    PlusEq,
    MinusEq,
    Eq,
    EqEq,
    EqEqEq,
    BangEq,
    BangEqEq,
    Lt,
    Le,
    Gt,
    Ge,
    Bang,
    AndAnd,
    OrOr,
    NullishCoalesce,
    OptionalDot,
    DotDotDot,
    Arrow,
    Question,
    Colon,
    Dot,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Eof,
}

fn can_start_regex(prev: Option<&Tok>) -> bool {
    matches!(
        prev,
        None | Some(
            Tok::Eq
                | Tok::LParen
                | Tok::LBracket
                | Tok::LBrace
                | Tok::Comma
                | Tok::Semi
                | Tok::Colon
                | Tok::Question
                | Tok::KwReturn
                | Tok::KwThrow
                | Tok::KwCase
                | Tok::Bang
                | Tok::AndAnd
                | Tok::OrOr
                | Tok::NullishCoalesce
                | Tok::Arrow
                | Tok::Plus
                | Tok::Minus
                | Tok::Star
                | Tok::Slash
                | Tok::EqEq
                | Tok::EqEqEq
                | Tok::BangEq
                | Tok::BangEqEq
        )
    )
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();

    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if b == b'/' && can_start_regex(out.last()) {
            i += 1;
            let mut pat = String::new();
            let mut in_class = false;
            while i < bytes.len() && (bytes[i] != b'/' || in_class) {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    pat.push(bytes[i] as char);
                    pat.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                if bytes[i] == b'[' {
                    in_class = true;
                } else if bytes[i] == b']' {
                    in_class = false;
                }
                let ch = src[i..].chars().next().ok_or("regex sin cerrar")?;
                pat.push(ch);
                i += ch.len_utf8();
            }
            if i >= bytes.len() {
                return Err("regex sin cerrar".into());
            }
            i += 1;
            let mut flags = String::new();
            while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                flags.push(bytes[i] as char);
                i += 1;
            }
            out.push(Tok::Regex(pat, flags));
            continue;
        }
        if b == b'"' || b == b'\'' || b == b'`' {
            let quote = b;
            i += 1;
            let mut s = String::new();
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                    match bytes[i] {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        other => s.push(other as char),
                    }
                } else {
                    let ch = src[i..].chars().next().ok_or("cadena sin cerrar")?;
                    s.push(ch);
                    i += ch.len_utf8();
                    continue;
                }
                i += 1;
            }
            if i >= bytes.len() {
                return Err("cadena sin cerrar".into());
            }
            i += 1;
            if quote == b'`' && s.contains("${") {
                out.push(Tok::Template(s));
            } else {
                out.push(Tok::Str(s));
            }
            continue;
        }
        if b.is_ascii_digit() {
            let start = i;
            if b == b'0' && matches!(bytes.get(i + 1), Some(b'x' | b'X')) {
                i += 2;
                while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                    i += 1;
                }
                let n = u64::from_str_radix(&src[start + 2..i], 16)
                    .map_err(|_| format!("número inválido `{}`", &src[start..i]))?;
                out.push(Tok::Num(n as f64));
                continue;
            }
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let n: f64 = src[start..i]
                .parse()
                .map_err(|_| format!("número inválido `{}`", &src[start..i]))?;
            out.push(Tok::Num(n));
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' || b == b'$' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
            {
                i += 1;
            }
            let kw = &src[start..i];
            out.push(match kw {
                "let" => Tok::KwLet,
                "const" => Tok::KwConst,
                "var" => Tok::KwVar,
                "function" => Tok::KwFunction,
                "class" => Tok::KwClass,
                "extends" => Tok::KwExtends,
                "this" => Tok::KwThis,
                "super" => Tok::KwSuper,
                "of" => Tok::KwOf,
                "in" => Tok::KwIn,
                "return" => Tok::KwReturn,
                "if" => Tok::KwIf,
                "else" => Tok::KwElse,
                "while" => Tok::KwWhile,
                "for" => Tok::KwFor,
                "switch" => Tok::KwSwitch,
                "case" => Tok::KwCase,
                "default" => Tok::KwDefault,
                "break" => Tok::KwBreak,
                "continue" => Tok::KwContinue,
                "try" => Tok::KwTry,
                "catch" => Tok::KwCatch,
                "finally" => Tok::KwFinally,
                "throw" => Tok::KwThrow,
                "new" => Tok::KwNew,
                "typeof" => Tok::KwTypeof,
                "void" => Tok::KwVoid,
                "async" => Tok::KwAsync,
                "await" => Tok::KwAwait,
                "true" => Tok::KwTrue,
                "false" => Tok::KwFalse,
                "null" => Tok::KwNull,
                "undefined" => Tok::KwUndefined,
                _ => Tok::Ident(kw.to_owned()),
            });
            continue;
        }
        let rest = &src[i..];
        let (tok, len) = if rest.starts_with("===") {
            (Tok::EqEqEq, 3)
        } else if rest.starts_with("!==") {
            (Tok::BangEqEq, 3)
        } else if rest.starts_with("...") {
            (Tok::DotDotDot, 3)
        } else if rest.starts_with("==") {
            (Tok::EqEq, 2)
        } else if rest.starts_with("!=") {
            (Tok::BangEq, 2)
        } else if rest.starts_with("<=") {
            (Tok::Le, 2)
        } else if rest.starts_with(">=") {
            (Tok::Ge, 2)
        } else if rest.starts_with("&&") {
            (Tok::AndAnd, 2)
        } else if rest.starts_with("||") {
            (Tok::OrOr, 2)
        } else if rest.starts_with("??") {
            (Tok::NullishCoalesce, 2)
        } else if rest.starts_with("?.") {
            (Tok::OptionalDot, 2)
        } else if rest.starts_with("=>") {
            (Tok::Arrow, 2)
        } else if rest.starts_with("++") {
            (Tok::PlusPlus, 2)
        } else if rest.starts_with("--") {
            (Tok::MinusMinus, 2)
        } else if rest.starts_with("+=") {
            (Tok::PlusEq, 2)
        } else if rest.starts_with("-=") {
            (Tok::MinusEq, 2)
        } else {
            let t = match b {
                b'+' => Tok::Plus,
                b'-' => Tok::Minus,
                b'*' => Tok::Star,
                b'/' => Tok::Slash,
                b'%' => Tok::Percent,
                b'=' => Tok::Eq,
                b'<' => Tok::Lt,
                b'>' => Tok::Gt,
                b'!' => Tok::Bang,
                b'?' => Tok::Question,
                b':' => Tok::Colon,
                b'.' => Tok::Dot,
                b'(' => Tok::LParen,
                b')' => Tok::RParen,
                b'{' => Tok::LBrace,
                b'}' => Tok::RBrace,
                b'[' => Tok::LBracket,
                b']' => Tok::RBracket,
                b',' => Tok::Comma,
                b';' => Tok::Semi,
                other => return Err(format!("carácter inesperado `{}`", other as char)),
            };
            (t, 1)
        };
        out.push(tok);
        i += len;
    }
    out.push(Tok::Eof);
    Ok(out)
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ArrayElem {
    Item(Expr),
    Spread(Expr),
}

#[derive(Debug, Clone)]
enum ObjProp {
    Pair(String, Expr),
    Spread(Expr),
}

#[derive(Debug, Clone)]
enum Pattern {
    Ident(String),
    Array(Vec<Pattern>, Option<String>),
    Object(Vec<(String, String, Option<Expr>)>),
}

#[derive(Debug, Clone)]
enum Expr {
    Lit(JsVal),
    Regex(String, String),
    Var(String),
    Array(Vec<ArrayElem>),
    Object(Vec<ObjProp>),
    Index(Box<Expr>, Box<Expr>),
    Prop(Box<Expr>, String),
    OptionalProp(Box<Expr>, String),
    Assign(String, Box<Expr>),
    SetProp(Box<Expr>, String, Box<Expr>),
    SetPropCompound(Box<Expr>, String, BinOp, Box<Expr>),
    SetIndex(Box<Expr>, Box<Expr>, Box<Expr>),
    UpdateVar(String, f64, bool),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Nullish(Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(String, Vec<ArrayElem>),
    CallMethod(Box<Expr>, String, Vec<ArrayElem>),
    OptionalCallMethod(Box<Expr>, String, Vec<ArrayElem>),
    CallValue(Box<Expr>, Vec<ArrayElem>),
    New(String, Vec<ArrayElem>),
    Lambda(Vec<String>, Vec<Stmt>),
}

#[derive(Debug, Clone, Copy)]
enum UnaryOp {
    Neg,
    Not,
    Typeof,
    Void,
}

#[derive(Debug, Clone, Copy)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    StrictEq,
    Ne,
    StrictNe,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    And,
    Or,
}

#[derive(Debug, Clone)]
enum Stmt {
    VarDecl(Pattern, Option<Expr>),
    Block(Vec<Stmt>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    For(Option<Box<Stmt>>, Option<Expr>, Option<Expr>, Box<Stmt>),
    ForOf(Pattern, Expr, Box<Stmt>),
    ForIn(Pattern, Expr, Box<Stmt>),
    Switch(Expr, Vec<(Option<Expr>, Vec<Stmt>)>),
    Break,
    Continue,
    TryCatch(Vec<Stmt>, Option<String>, Vec<Stmt>, Vec<Stmt>),
    Throw(Expr),
    FnDecl(String, Vec<String>, Vec<Stmt>),
    ClassDecl(
        String,
        Option<String>,
        Option<(Vec<String>, Vec<Stmt>)>,
        Vec<(String, Vec<String>, Vec<Stmt>)>,
    ),
    Return(Option<Expr>),
    Expr(Expr),
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Self { toks, pos: 0 }
    }

    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).unwrap_or(&Tok::Eof)
    }

    fn peek_at(&self, offset: usize) -> &Tok {
        self.toks.get(self.pos + offset).unwrap_or(&Tok::Eof)
    }

    fn next(&mut self) -> Tok {
        let t = self.peek().clone();
        if !matches!(t, Tok::Eof) {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, expected: &Tok) -> bool {
        if self.peek() == expected {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &Tok) -> Result<(), String> {
        if self.eat(expected) {
            Ok(())
        } else {
            let got = self.peek();
            Err(format!("se esperaba `{expected:?}`, se encontró `{got:?}`"))
        }
    }

    fn is_arrow_params(&self) -> bool {
        if !matches!(self.peek(), Tok::LParen) {
            return false;
        }
        let mut idx = 1usize;
        if matches!(self.peek_at(idx), Tok::RParen) {
            return matches!(self.peek_at(idx + 1), Tok::Arrow);
        }
        loop {
            if !matches!(self.peek_at(idx), Tok::Ident(_)) {
                return false;
            }
            idx += 1;
            match self.peek_at(idx) {
                Tok::Comma => idx += 1,
                Tok::RParen => return matches!(self.peek_at(idx + 1), Tok::Arrow),
                _ => return false,
            }
        }
    }

    fn parse_program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut stmts = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            while self.eat(&Tok::Semi) {}
            if matches!(self.peek(), Tok::Eof) {
                break;
            }
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
    }

    fn parse_block_stmts(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect(&Tok::LBrace)?;
        let mut body = Vec::new();
        while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
            while self.eat(&Tok::Semi) {}
            if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                break;
            }
            body.push(self.parse_stmt()?);
        }
        self.expect(&Tok::RBrace)?;
        Ok(body)
    }

    fn parse_pattern(&mut self) -> Result<Pattern, String> {
        match self.next() {
            Tok::Ident(n) => Ok(Pattern::Ident(n)),
            Tok::LBracket => {
                let mut items = Vec::new();
                let mut rest = None;
                while !matches!(self.peek(), Tok::RBracket | Tok::Eof) {
                    if self.eat(&Tok::DotDotDot) {
                        let name = match self.next() {
                            Tok::Ident(n) => n,
                            other => return Err(format!("identificador rest inválido: {other:?}")),
                        };
                        rest = Some(name);
                        let _ = self.eat(&Tok::Comma);
                        break;
                    }
                    items.push(self.parse_pattern()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBracket)?;
                Ok(Pattern::Array(items, rest))
            }
            Tok::LBrace => {
                let mut props = Vec::new();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    let key = match self.next() {
                        Tok::Ident(n) => n,
                        other => return Err(format!("clave destructuring inválida: {other:?}")),
                    };
                    let bind = if self.eat(&Tok::Colon) {
                        match self.next() {
                            Tok::Ident(n) => n,
                            other => return Err(format!("alias destructuring inválido: {other:?}")),
                        }
                    } else {
                        key.clone()
                    };
                    let default = if self.eat(&Tok::Eq) {
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    props.push((key, bind, default));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBrace)?;
                Ok(Pattern::Object(props))
            }
            other => Err(format!(
                "se esperaba identificador o patrón tras declaración, no {other:?}"
            )),
        }
    }

    fn parse_var_decl(&mut self, consume_semi: bool) -> Result<Stmt, String> {
        self.next();
        let pat = self.parse_pattern()?;
        let init = if self.eat(&Tok::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        if consume_semi {
            let _ = self.eat(&Tok::Semi);
        }
        Ok(Stmt::VarDecl(pat, init))
    }

    fn parse_stmt(&mut self) -> Result<Stmt, String> {
        while self.eat(&Tok::Semi) {}
        match self.peek().clone() {
            Tok::KwAsync => {
                self.next();
                if matches!(self.peek(), Tok::KwFunction) {
                    self.parse_stmt()
                } else {
                    let e = self.parse_expr()?;
                    let _ = self.eat(&Tok::Semi);
                    Ok(Stmt::Expr(e))
                }
            }
            Tok::KwLet | Tok::KwConst | Tok::KwVar => self.parse_var_decl(true),
            Tok::KwFunction => {
                self.next();
                let name = match self.next() {
                    Tok::Ident(n) => n,
                    other => return Err(format!("nombre de función inválido: {other:?}")),
                };
                let params = self.parse_param_list()?;
                let body = self.parse_block_stmts()?;
                Ok(Stmt::FnDecl(name, params, body))
            }
            Tok::KwClass => {
                self.next();
                let name = match self.next() {
                    Tok::Ident(n) => n,
                    other => return Err(format!("nombre de clase inválido: {other:?}")),
                };
                let parent = if self.eat(&Tok::KwExtends) {
                    match self.next() {
                        Tok::Ident(p) => Some(p),
                        other => return Err(format!("superclase inválida: {other:?}")),
                    }
                } else {
                    None
                };
                self.expect(&Tok::LBrace)?;
                let mut ctor = None;
                let mut methods = Vec::new();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    while self.eat(&Tok::Semi) {}
                    if matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                        break;
                    }
                    let mname = match tok_as_prop_name(self.next()) {
                        Some(n) => n,
                        None => return Err("nombre de método inválido en class".into()),
                    };
                    let params = self.parse_param_list()?;
                    let body = self.parse_block_stmts()?;
                    if mname == "constructor" {
                        ctor = Some((params, body));
                    } else {
                        methods.push((mname, params, body));
                    }
                }
                self.expect(&Tok::RBrace)?;
                Ok(Stmt::ClassDecl(name, parent, ctor, methods))
            }
            Tok::KwReturn => {
                self.next();
                let val = if matches!(self.peek(), Tok::Semi | Tok::RBrace | Tok::Eof) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                let _ = self.eat(&Tok::Semi);
                Ok(Stmt::Return(val))
            }
            Tok::KwIf => {
                self.next();
                self.expect(&Tok::LParen)?;
                let cond = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                let then_b = Box::new(self.parse_stmt()?);
                let else_b = if self.eat(&Tok::KwElse) {
                    Some(Box::new(self.parse_stmt()?))
                } else {
                    None
                };
                Ok(Stmt::If(cond, then_b, else_b))
            }
            Tok::KwWhile => {
                self.next();
                self.expect(&Tok::LParen)?;
                let cond = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                let body = Box::new(self.parse_stmt()?);
                Ok(Stmt::While(cond, body))
            }
            Tok::KwFor => {
                self.next();
                self.expect(&Tok::LParen)?;
                if matches!(self.peek(), Tok::KwLet | Tok::KwConst | Tok::KwVar) {
                    let saved_pos = self.pos;
                    self.next();
                    if let Ok(pat) = self.parse_pattern() {
                        if self.eat(&Tok::KwOf) {
                            let iter = self.parse_expr()?;
                            self.expect(&Tok::RParen)?;
                            let body = Box::new(self.parse_stmt()?);
                            return Ok(Stmt::ForOf(pat, iter, body));
                        }
                        if self.eat(&Tok::KwIn) {
                            let obj = self.parse_expr()?;
                            self.expect(&Tok::RParen)?;
                            let body = Box::new(self.parse_stmt()?);
                            return Ok(Stmt::ForIn(pat, obj, body));
                        }
                    }
                    self.pos = saved_pos;
                }
                let init = if self.eat(&Tok::Semi) {
                    None
                } else if matches!(self.peek(), Tok::KwLet | Tok::KwConst | Tok::KwVar) {
                    let s = self.parse_var_decl(false)?;
                    self.expect(&Tok::Semi)?;
                    Some(Box::new(s))
                } else {
                    let e = self.parse_expr()?;
                    self.expect(&Tok::Semi)?;
                    Some(Box::new(Stmt::Expr(e)))
                };
                let cond = if self.eat(&Tok::Semi) {
                    None
                } else {
                    let e = self.parse_expr()?;
                    self.expect(&Tok::Semi)?;
                    Some(e)
                };
                let step = if matches!(self.peek(), Tok::RParen) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.expect(&Tok::RParen)?;
                let body = Box::new(self.parse_stmt()?);
                Ok(Stmt::For(init, cond, step, body))
            }
            Tok::KwSwitch => {
                self.next();
                self.expect(&Tok::LParen)?;
                let disc = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                self.expect(&Tok::LBrace)?;
                let mut cases = Vec::new();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    let test = if self.eat(&Tok::KwCase) {
                        let e = self.parse_expr()?;
                        self.expect(&Tok::Colon)?;
                        Some(e)
                    } else if self.eat(&Tok::KwDefault) {
                        self.expect(&Tok::Colon)?;
                        None
                    } else {
                        return Err("se esperaba `case` o `default` en switch".into());
                    };
                    let mut body = Vec::new();
                    while !matches!(
                        self.peek(),
                        Tok::KwCase | Tok::KwDefault | Tok::RBrace | Tok::Eof
                    ) {
                        body.push(self.parse_stmt()?);
                    }
                    cases.push((test, body));
                }
                self.expect(&Tok::RBrace)?;
                Ok(Stmt::Switch(disc, cases))
            }
            Tok::KwBreak => {
                self.next();
                let _ = self.eat(&Tok::Semi);
                Ok(Stmt::Break)
            }
            Tok::KwContinue => {
                self.next();
                let _ = self.eat(&Tok::Semi);
                Ok(Stmt::Continue)
            }
            Tok::KwTry => {
                self.next();
                let try_body = self.parse_block_stmts()?;
                let (catch_var, catch_body) = if self.eat(&Tok::KwCatch) {
                    let cv = if self.eat(&Tok::LParen) {
                        let name = match self.next() {
                            Tok::Ident(n) => n,
                            other => return Err(format!("identificador catch inválido: {other:?}")),
                        };
                        self.expect(&Tok::RParen)?;
                        Some(name)
                    } else {
                        None
                    };
                    (cv, self.parse_block_stmts()?)
                } else {
                    (None, Vec::new())
                };
                let finally_body = if self.eat(&Tok::KwFinally) {
                    self.parse_block_stmts()?
                } else {
                    Vec::new()
                };
                Ok(Stmt::TryCatch(
                    try_body,
                    catch_var,
                    catch_body,
                    finally_body,
                ))
            }
            Tok::KwThrow => {
                self.next();
                let val = self.parse_expr()?;
                let _ = self.eat(&Tok::Semi);
                Ok(Stmt::Throw(val))
            }
            Tok::LBrace => Ok(Stmt::Block(self.parse_block_stmts()?)),
            _ => {
                let e = self.parse_expr()?;
                let _ = self.eat(&Tok::Semi);
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn parse_param_list(&mut self) -> Result<Vec<String>, String> {
        self.expect(&Tok::LParen)?;
        let mut params = Vec::new();
        while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
            if self.eat(&Tok::DotDotDot) {
                if let Tok::Ident(p) = self.next() {
                    params.push(format!("...{p}"));
                } else {
                    return Err("parámetro rest inválido".into());
                }
                let _ = self.eat(&Tok::Comma);
                break;
            }
            if let Tok::Ident(p) = self.next() {
                if self.eat(&Tok::Eq) {
                    let _ = self.parse_assign()?;
                }
                params.push(p);
            } else {
                return Err("parámetro inválido".into());
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(params)
    }

    fn parse_arrow_body(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect(&Tok::Arrow)?;
        if matches!(self.peek(), Tok::LBrace) {
            self.parse_block_stmts()
        } else {
            let expr = self.parse_assign()?;
            Ok(vec![Stmt::Return(Some(expr))])
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_ternary()?;
        if self.eat(&Tok::Eq) {
            let rhs = self.parse_assign()?;
            return match lhs {
                Expr::Var(name) => Ok(Expr::Assign(name, Box::new(rhs))),
                Expr::Prop(obj, prop) => Ok(Expr::SetProp(obj, prop, Box::new(rhs))),
                Expr::Index(arr, idx) => Ok(Expr::SetIndex(arr, idx, Box::new(rhs))),
                _ => Err("asignación inválida".into()),
            };
        }
        if self.eat(&Tok::PlusEq) || self.eat(&Tok::MinusEq) {
            let is_add = matches!(self.toks.get(self.pos.saturating_sub(1)), Some(Tok::PlusEq));
            let op = if is_add { BinOp::Add } else { BinOp::Sub };
            let rhs = self.parse_assign()?;
            return match lhs {
                Expr::Var(name) => Ok(Expr::Assign(
                    name.clone(),
                    Box::new(Expr::Binary(op, Box::new(Expr::Var(name)), Box::new(rhs))),
                )),
                Expr::Prop(obj, prop) => Ok(Expr::SetPropCompound(obj, prop, op, Box::new(rhs))),
                _ => Err("asignación compuesta inválida".into()),
            };
        }
        Ok(lhs)
    }

    fn parse_ternary(&mut self) -> Result<Expr, String> {
        let cond = self.parse_nullish()?;
        if self.eat(&Tok::Question) {
            let then_e = self.parse_assign()?;
            self.expect(&Tok::Colon)?;
            let else_e = self.parse_assign()?;
            return Ok(Expr::Ternary(
                Box::new(cond),
                Box::new(then_e),
                Box::new(else_e),
            ));
        }
        Ok(cond)
    }

    fn parse_nullish(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_or()?;
        while self.eat(&Tok::NullishCoalesce) {
            let rhs = self.parse_or()?;
            lhs = Expr::Nullish(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_and()?;
        while self.eat(&Tok::OrOr) {
            let rhs = self.parse_and()?;
            lhs = Expr::Binary(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_eq()?;
        while self.eat(&Tok::AndAnd) {
            let rhs = self.parse_eq()?;
            lhs = Expr::Binary(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_rel()?;
        loop {
            let op = match self.peek() {
                Tok::EqEq => BinOp::Eq,
                Tok::EqEqEq => BinOp::StrictEq,
                Tok::BangEq => BinOp::Ne,
                Tok::BangEqEq => BinOp::StrictNe,
                _ => break,
            };
            self.next();
            let rhs = self.parse_rel()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_rel(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_add()?;
        loop {
            let op = match self.peek() {
                Tok::Lt => BinOp::Lt,
                Tok::Le => BinOp::Le,
                Tok::Gt => BinOp::Gt,
                Tok::Ge => BinOp::Ge,
                Tok::KwIn => BinOp::In,
                _ => break,
            };
            self.next();
            let rhs = self.parse_add()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.next();
            let rhs = self.parse_mul()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, String> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Rem,
                _ => break,
            };
            self.next();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Tok::Minus => {
                self.next();
                Ok(Expr::Unary(UnaryOp::Neg, Box::new(self.parse_unary()?)))
            }
            Tok::Plus => {
                self.next();
                self.parse_unary()
            }
            Tok::Bang => {
                self.next();
                Ok(Expr::Unary(UnaryOp::Not, Box::new(self.parse_unary()?)))
            }
            Tok::KwTypeof => {
                self.next();
                Ok(Expr::Unary(UnaryOp::Typeof, Box::new(self.parse_unary()?)))
            }
            Tok::KwVoid => {
                self.next();
                Ok(Expr::Unary(UnaryOp::Void, Box::new(self.parse_unary()?)))
            }
            Tok::KwAwait => {
                self.next();
                self.parse_unary()
            }
            Tok::PlusPlus | Tok::MinusMinus => {
                let delta = if matches!(self.next(), Tok::PlusPlus) {
                    1.0
                } else {
                    -1.0
                };
                match self.parse_postfix()? {
                    Expr::Var(name) => Ok(Expr::UpdateVar(name, delta, false)),
                    _ => Err("incremento inválido".into()),
                }
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_arg_list(&mut self) -> Result<Vec<ArrayElem>, String> {
        let mut args = Vec::new();
        while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
            if self.eat(&Tok::DotDotDot) {
                args.push(ArrayElem::Spread(self.parse_expr()?));
            } else {
                args.push(ArrayElem::Item(self.parse_expr()?));
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(args)
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.eat(&Tok::LParen) {
                let args = self.parse_arg_list()?;
                expr = match expr {
                    Expr::Var(name) => Expr::Call(name, args),
                    Expr::Prop(obj, method) => Expr::CallMethod(obj, method, args),
                    Expr::OptionalProp(obj, method) => Expr::OptionalCallMethod(obj, method, args),
                    other => Expr::CallValue(Box::new(other), args),
                };
            } else if self.eat(&Tok::Dot) {
                let prop = match tok_as_prop_name(self.next()) {
                    Some(p) => p,
                    None => return Err("propiedad inválida tras `.`".into()),
                };
                expr = Expr::Prop(Box::new(expr), prop);
            } else if self.eat(&Tok::OptionalDot) {
                let prop = match tok_as_prop_name(self.next()) {
                    Some(p) => p,
                    None => return Err("propiedad inválida tras `?.`".into()),
                };
                expr = Expr::OptionalProp(Box::new(expr), prop);
            } else if self.eat(&Tok::LBracket) {
                let idx = self.parse_expr()?;
                self.expect(&Tok::RBracket)?;
                expr = Expr::Index(Box::new(expr), Box::new(idx));
            } else if self.eat(&Tok::PlusPlus) {
                if let Expr::Var(name) = expr {
                    expr = Expr::UpdateVar(name, 1.0, true);
                } else {
                    return Err("incremento inválido".into());
                }
            } else if self.eat(&Tok::MinusMinus) {
                if let Expr::Var(name) = expr {
                    expr = Expr::UpdateVar(name, -1.0, true);
                } else {
                    return Err("decremento inválido".into());
                }
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_template(&self, raw: &str) -> Result<Expr, String> {
        let mut parts: Vec<Expr> = Vec::new();
        let mut rest = raw;
        while let Some(start) = rest.find("${") {
            let prefix = &rest[..start];
            if !prefix.is_empty() {
                parts.push(Expr::Lit(JsVal::Str(prefix.to_owned())));
            }
            let after = &rest[start + 2..];
            let mut depth = 1usize;
            let mut end_idx = None;
            for (idx, ch) in after.char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end_idx = Some(idx);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let end = end_idx.ok_or("interpolación `${...}` sin cerrar")?;
            let expr_src = &after[..end];
            let sub_toks = tokenize(expr_src)?;
            let sub_expr = Parser::new(sub_toks).parse_expr()?;
            parts.push(sub_expr);
            rest = &after[end + 1..];
        }
        if !rest.is_empty() || parts.is_empty() {
            parts.push(Expr::Lit(JsVal::Str(rest.to_owned())));
        }
        let mut acc = if matches!(parts.first(), Some(Expr::Lit(JsVal::Str(_)))) {
            parts.remove(0)
        } else {
            Expr::Lit(JsVal::Str(String::new()))
        };
        for part in parts {
            acc = Expr::Binary(BinOp::Add, Box::new(acc), Box::new(part));
        }
        Ok(acc)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        if self.is_arrow_params() {
            let params = self.parse_param_list()?;
            let body = self.parse_arrow_body()?;
            return Ok(Expr::Lambda(params, body));
        }
        match self.next() {
            Tok::Num(n) => Ok(Expr::Lit(JsVal::Number(n))),
            Tok::Str(s) => Ok(Expr::Lit(JsVal::Str(s))),
            Tok::Template(raw) => self.parse_template(&raw),
            Tok::Regex(pat, flags) => Ok(Expr::Regex(pat, flags)),
            Tok::KwTrue => Ok(Expr::Lit(JsVal::Bool(true))),
            Tok::KwFalse => Ok(Expr::Lit(JsVal::Bool(false))),
            Tok::KwNull => Ok(Expr::Lit(JsVal::Null)),
            Tok::KwUndefined => Ok(Expr::Lit(JsVal::Undefined)),
            Tok::KwThis => Ok(Expr::Var("this".to_owned())),
            Tok::KwSuper => Ok(Expr::Var("super".to_owned())),
            Tok::KwAsync => {
                if self.eat(&Tok::KwFunction) {
                    if matches!(self.peek(), Tok::Ident(_)) {
                        let _ = self.next();
                    }
                    let params = self.parse_param_list()?;
                    let body = self.parse_block_stmts()?;
                    Ok(Expr::Lambda(params, body))
                } else {
                    self.parse_primary()
                }
            }
            Tok::KwFunction => {
                if matches!(self.peek(), Tok::Ident(_)) {
                    let _ = self.next();
                }
                let params = self.parse_param_list()?;
                let body = self.parse_block_stmts()?;
                Ok(Expr::Lambda(params, body))
            }
            Tok::KwNew => {
                let ctor = match self.next() {
                    Tok::Ident(name) => name,
                    other => return Err(format!("constructor inválido tras `new`: {other:?}")),
                };
                let args = if self.eat(&Tok::LParen) {
                    self.parse_arg_list()?
                } else {
                    Vec::new()
                };
                Ok(Expr::New(ctor, args))
            }
            Tok::Ident(name) => {
                if matches!(self.peek(), Tok::Arrow) {
                    let body = self.parse_arrow_body()?;
                    Ok(Expr::Lambda(vec![name], body))
                } else {
                    Ok(Expr::Var(name))
                }
            }
            Tok::LParen => {
                let e = self.parse_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Tok::LBracket => {
                let mut items = Vec::new();
                while !matches!(self.peek(), Tok::RBracket | Tok::Eof) {
                    if self.eat(&Tok::DotDotDot) {
                        items.push(ArrayElem::Spread(self.parse_expr()?));
                    } else {
                        items.push(ArrayElem::Item(self.parse_expr()?));
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBracket)?;
                Ok(Expr::Array(items))
            }
            Tok::LBrace => {
                let mut pairs = Vec::new();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    if self.eat(&Tok::DotDotDot) {
                        pairs.push(ObjProp::Spread(self.parse_expr()?));
                    } else {
                        let key = match self.next() {
                            Tok::Ident(k) | Tok::Str(k) => k,
                            other => return Err(format!("clave de objeto inválida: {other:?}")),
                        };
                        if self.eat(&Tok::Colon) {
                            let val = self.parse_expr()?;
                            pairs.push(ObjProp::Pair(key, val));
                        } else {
                            pairs.push(ObjProp::Pair(key.clone(), Expr::Var(key)));
                        }
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RBrace)?;
                Ok(Expr::Object(pairs))
            }
            other => Err(format!("expresión inesperada: {other:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Bytecode + Compilador + VM de pila
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Push(JsVal),
    Pop,
    Dup,
    Load(String),
    Store(String),
    BindPattern(Pattern),
    MakeRegex(String, String),
    MakeArray(usize),
    MakeArraySpread(Vec<bool>),
    MakeObjectSpread(Vec<(Option<String>, bool)>),
    IndexGet,
    IndexSet,
    PropGet(String),
    OptionalPropGet(String),
    PropSet(String),
    PropCompound(String, BinOp),
    Unary(UnaryOp),
    Binary(BinOp),
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfNullish(usize),
    IterKeys,
    TryStart(usize),
    TryEnd,
    Throw,
    BindVar(String),
    Call(String, Vec<bool>),
    CallMethod(String, Vec<bool>, Option<String>),
    OptionalCallMethod(String, Vec<bool>),
    CallValue(Vec<bool>),
    Construct(String, Vec<bool>),
    RegisterClass {
        name: String,
        parent: Option<String>,
        ctor: Option<usize>,
        methods: Vec<(String, usize)>,
    },
    Return,
}

#[derive(Debug, Clone)]
struct FunctionChunk {
    params: Vec<String>,
    code: Vec<Op>,
}

#[derive(Debug, Clone)]
struct ClassDef {
    parent: Option<String>,
    ctor: Option<usize>,
    methods: HashMap<String, usize>,
}

struct LoopContext {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

struct Compiler {
    functions: Vec<FunctionChunk>,
    fn_names: HashMap<String, usize>,
    loops: Vec<LoopContext>,
    temp_counter: usize,
}

impl Compiler {
    fn new() -> Self {
        Self {
            functions: Vec::new(),
            fn_names: HashMap::new(),
            loops: Vec::new(),
            temp_counter: 0,
        }
    }

    fn next_temp(&mut self, prefix: &str) -> String {
        self.temp_counter += 1;
        format!("__wisp_{prefix}_{}", self.temp_counter)
    }

    fn compile_program(&mut self, stmts: &[Stmt]) -> Vec<Op> {
        let mut code = Vec::new();
        for (idx, stmt) in stmts.iter().enumerate() {
            let is_last = idx + 1 == stmts.len();
            self.compile_stmt(stmt, &mut code, is_last);
        }
        if code.is_empty() {
            code.push(Op::Push(JsVal::Undefined));
        }
        code
    }

    fn compile_stmt(&mut self, stmt: &Stmt, code: &mut Vec<Op>, keep_value: bool) {
        match stmt {
            Stmt::VarDecl(pat, init) => {
                if let Some(expr) = init {
                    self.compile_expr(expr, code);
                } else {
                    code.push(Op::Push(JsVal::Undefined));
                }
                match pat {
                    Pattern::Ident(name) => code.push(Op::Store(name.clone())),
                    other => code.push(Op::BindPattern(other.clone())),
                }
                if !keep_value {
                    code.push(Op::Pop);
                }
            }
            Stmt::Block(stmts) => {
                if stmts.is_empty() {
                    if keep_value {
                        code.push(Op::Push(JsVal::Undefined));
                    }
                    return;
                }
                for (i, s) in stmts.iter().enumerate() {
                    self.compile_stmt(s, code, keep_value && i + 1 == stmts.len());
                }
            }
            Stmt::If(cond, then_b, else_b) => {
                self.compile_expr(cond, code);
                let j_false = code.len();
                code.push(Op::JumpIfFalse(0));
                self.compile_stmt(then_b, code, keep_value);
                if let Some(else_stmt) = else_b {
                    let j_end = code.len();
                    code.push(Op::Jump(0));
                    code[j_false] = Op::JumpIfFalse(code.len());
                    self.compile_stmt(else_stmt, code, keep_value);
                    code[j_end] = Op::Jump(code.len());
                } else {
                    code[j_false] = Op::JumpIfFalse(code.len());
                    if keep_value {
                        code.push(Op::Push(JsVal::Undefined));
                    }
                }
            }
            Stmt::While(cond, body) => {
                let loop_start = code.len();
                self.loops.push(LoopContext {
                    breaks: Vec::new(),
                    continues: Vec::new(),
                });
                self.compile_expr(cond, code);
                let j_exit = code.len();
                code.push(Op::JumpIfFalse(0));
                self.compile_stmt(body, code, false);
                code.push(Op::Jump(loop_start));
                let loop_end = code.len();
                code[j_exit] = Op::JumpIfFalse(loop_end);
                if let Some(ctx) = self.loops.pop() {
                    for b in ctx.breaks {
                        code[b] = Op::Jump(loop_end);
                    }
                    for c in ctx.continues {
                        code[c] = Op::Jump(loop_start);
                    }
                }
                if keep_value {
                    code.push(Op::Push(JsVal::Undefined));
                }
            }
            Stmt::For(init, cond, step, body) => {
                if let Some(init_stmt) = init {
                    self.compile_stmt(init_stmt, code, false);
                }
                let loop_start = code.len();
                self.loops.push(LoopContext {
                    breaks: Vec::new(),
                    continues: Vec::new(),
                });
                let j_exit = if let Some(cond_expr) = cond {
                    self.compile_expr(cond_expr, code);
                    let pos = code.len();
                    code.push(Op::JumpIfFalse(0));
                    Some(pos)
                } else {
                    None
                };
                self.compile_stmt(body, code, false);
                let step_start = code.len();
                if let Some(step_expr) = step {
                    self.compile_expr(step_expr, code);
                    code.push(Op::Pop);
                }
                code.push(Op::Jump(loop_start));
                let loop_end = code.len();
                if let Some(pos) = j_exit {
                    code[pos] = Op::JumpIfFalse(loop_end);
                }
                if let Some(ctx) = self.loops.pop() {
                    for b in ctx.breaks {
                        code[b] = Op::Jump(loop_end);
                    }
                    for c in ctx.continues {
                        code[c] = Op::Jump(step_start);
                    }
                }
                if keep_value {
                    code.push(Op::Push(JsVal::Undefined));
                }
            }
            Stmt::ForOf(pat, iter_expr, body) | Stmt::ForIn(pat, iter_expr, body) => {
                let is_for_in = matches!(stmt, Stmt::ForIn(..));
                let arr_var = self.next_temp("iter");
                let idx_var = self.next_temp("idx");
                self.compile_expr(iter_expr, code);
                if is_for_in {
                    code.push(Op::IterKeys);
                }
                code.push(Op::Store(arr_var.clone()));
                code.push(Op::Pop);
                code.push(Op::Push(JsVal::Number(0.0)));
                code.push(Op::Store(idx_var.clone()));
                code.push(Op::Pop);

                let loop_start = code.len();
                self.loops.push(LoopContext {
                    breaks: Vec::new(),
                    continues: Vec::new(),
                });
                code.push(Op::Load(idx_var.clone()));
                code.push(Op::Load(arr_var.clone()));
                code.push(Op::PropGet("length".to_owned()));
                code.push(Op::Binary(BinOp::Lt));
                let j_exit = code.len();
                code.push(Op::JumpIfFalse(0));

                code.push(Op::Load(arr_var));
                code.push(Op::Load(idx_var.clone()));
                code.push(Op::IndexGet);
                match pat {
                    Pattern::Ident(name) => code.push(Op::Store(name.clone())),
                    other => code.push(Op::BindPattern(other.clone())),
                }
                code.push(Op::Pop);

                self.compile_stmt(body, code, false);
                let step_start = code.len();
                code.push(Op::Load(idx_var.clone()));
                code.push(Op::Push(JsVal::Number(1.0)));
                code.push(Op::Binary(BinOp::Add));
                code.push(Op::Store(idx_var));
                code.push(Op::Pop);
                code.push(Op::Jump(loop_start));
                let loop_end = code.len();
                code[j_exit] = Op::JumpIfFalse(loop_end);
                if let Some(ctx) = self.loops.pop() {
                    for b in ctx.breaks {
                        code[b] = Op::Jump(loop_end);
                    }
                    for c in ctx.continues {
                        code[c] = Op::Jump(step_start);
                    }
                }
                if keep_value {
                    code.push(Op::Push(JsVal::Undefined));
                }
            }
            Stmt::Switch(disc, cases) => {
                let disc_var = self.next_temp("sw");
                self.compile_expr(disc, code);
                code.push(Op::Store(disc_var.clone()));
                code.push(Op::Pop);
                self.loops.push(LoopContext {
                    breaks: Vec::new(),
                    continues: Vec::new(),
                });
                let mut body_jumps = Vec::new();
                let mut default_jump = None;
                for (i, (test, _)) in cases.iter().enumerate() {
                    if let Some(test_expr) = test {
                        code.push(Op::Load(disc_var.clone()));
                        self.compile_expr(test_expr, code);
                        code.push(Op::Binary(BinOp::StrictEq));
                        let j_next = code.len();
                        code.push(Op::JumpIfFalse(0));
                        let j_body = code.len();
                        code.push(Op::Jump(0));
                        code[j_next] = Op::JumpIfFalse(code.len());
                        body_jumps.push((i, j_body));
                    } else {
                        default_jump = Some((i, code.len()));
                    }
                }
                let j_no_match = code.len();
                code.push(Op::Jump(0));
                let mut body_starts = vec![0usize; cases.len()];
                for (i, (_, body)) in cases.iter().enumerate() {
                    body_starts[i] = code.len();
                    for s in body {
                        self.compile_stmt(s, code, false);
                    }
                }
                let end_ip = code.len();
                for (i, j_pos) in body_jumps {
                    code[j_pos] = Op::Jump(body_starts[i]);
                }
                if let Some((i, _)) = default_jump {
                    code[j_no_match] = Op::Jump(body_starts[i]);
                } else {
                    code[j_no_match] = Op::Jump(end_ip);
                }
                if let Some(ctx) = self.loops.pop() {
                    for b in ctx.breaks {
                        code[b] = Op::Jump(end_ip);
                    }
                    if let Some(outer) = self.loops.last_mut() {
                        outer.continues.extend(ctx.continues);
                    }
                }
                if keep_value {
                    code.push(Op::Push(JsVal::Undefined));
                }
            }
            Stmt::Break => {
                let pos = code.len();
                code.push(Op::Jump(0));
                if let Some(ctx) = self.loops.last_mut() {
                    ctx.breaks.push(pos);
                }
            }
            Stmt::Continue => {
                let pos = code.len();
                code.push(Op::Jump(0));
                if let Some(ctx) = self.loops.last_mut() {
                    ctx.continues.push(pos);
                }
            }
            Stmt::TryCatch(try_body, catch_var, catch_body, finally_body) => {
                let try_pos = code.len();
                code.push(Op::TryStart(0));
                for s in try_body {
                    self.compile_stmt(s, code, false);
                }
                code.push(Op::TryEnd);
                let j_after = code.len();
                code.push(Op::Jump(0));
                let catch_start = code.len();
                code[try_pos] = Op::TryStart(catch_start);
                if let Some(var) = catch_var {
                    code.push(Op::BindVar(var.clone()));
                } else {
                    code.push(Op::Pop);
                }
                for s in catch_body {
                    self.compile_stmt(s, code, false);
                }
                code[j_after] = Op::Jump(code.len());
                for s in finally_body {
                    self.compile_stmt(s, code, false);
                }
                if keep_value {
                    code.push(Op::Push(JsVal::Undefined));
                }
            }
            Stmt::Throw(expr) => {
                self.compile_expr(expr, code);
                code.push(Op::Throw);
            }
            Stmt::FnDecl(name, params, body) => {
                let id = self.compile_fn(params, body);
                self.fn_names.insert(name.clone(), id);
                code.push(Op::Push(JsVal::Function(id)));
                code.push(Op::Store(name.clone()));
                if !keep_value {
                    code.push(Op::Pop);
                }
            }
            Stmt::ClassDecl(name, parent, ctor, methods) => {
                let ctor_id = ctor.as_ref().map(|(p, b)| self.compile_fn(p, b));
                let mut compiled_methods = Vec::with_capacity(methods.len());
                for (mname, params, body) in methods {
                    let mid = self.compile_fn(params, body);
                    compiled_methods.push((mname.clone(), mid));
                }
                code.push(Op::RegisterClass {
                    name: name.clone(),
                    parent: parent.clone(),
                    ctor: ctor_id,
                    methods: compiled_methods,
                });
                if !keep_value {
                    code.push(Op::Pop);
                }
            }
            Stmt::Return(expr) => {
                if let Some(e) = expr {
                    self.compile_expr(e, code);
                } else {
                    code.push(Op::Push(JsVal::Undefined));
                }
                code.push(Op::Return);
            }
            Stmt::Expr(expr) => {
                self.compile_expr(expr, code);
                if !keep_value {
                    code.push(Op::Pop);
                }
            }
        }
    }

    fn compile_fn(&mut self, params: &[String], body: &[Stmt]) -> usize {
        let mut fn_code = Vec::new();
        for s in body {
            self.compile_stmt(s, &mut fn_code, false);
        }
        fn_code.push(Op::Push(JsVal::Undefined));
        fn_code.push(Op::Return);
        let id = self.functions.len();
        self.functions.push(FunctionChunk {
            params: params.to_vec(),
            code: fn_code,
        });
        id
    }

    fn compile_arg_elems(&mut self, elems: &[ArrayElem], code: &mut Vec<Op>) -> Vec<bool> {
        let mut spreads = Vec::with_capacity(elems.len());
        for el in elems {
            match el {
                ArrayElem::Item(e) => {
                    self.compile_expr(e, code);
                    spreads.push(false);
                }
                ArrayElem::Spread(e) => {
                    self.compile_expr(e, code);
                    spreads.push(true);
                }
            }
        }
        spreads
    }

    fn compile_expr(&mut self, expr: &Expr, code: &mut Vec<Op>) {
        match expr {
            Expr::Lit(val) => code.push(Op::Push(val.clone())),
            Expr::Regex(pat, flags) => code.push(Op::MakeRegex(pat.clone(), flags.clone())),
            Expr::Var(name) => code.push(Op::Load(name.clone())),
            Expr::Array(items) => {
                let has_spread = items.iter().any(|x| matches!(x, ArrayElem::Spread(_)));
                if !has_spread {
                    for el in items {
                        if let ArrayElem::Item(e) = el {
                            self.compile_expr(e, code);
                        }
                    }
                    code.push(Op::MakeArray(items.len()));
                } else {
                    let spreads = self.compile_arg_elems(items, code);
                    code.push(Op::MakeArraySpread(spreads));
                }
            }
            Expr::Object(props) => {
                let mut spec = Vec::with_capacity(props.len());
                for p in props {
                    match p {
                        ObjProp::Pair(k, v) => {
                            self.compile_expr(v, code);
                            spec.push((Some(k.clone()), false));
                        }
                        ObjProp::Spread(e) => {
                            self.compile_expr(e, code);
                            spec.push((None, true));
                        }
                    }
                }
                code.push(Op::MakeObjectSpread(spec));
            }
            Expr::Index(target, index) => {
                self.compile_expr(target, code);
                self.compile_expr(index, code);
                code.push(Op::IndexGet);
            }
            Expr::Prop(target, prop) => {
                self.compile_expr(target, code);
                code.push(Op::PropGet(prop.clone()));
            }
            Expr::OptionalProp(target, prop) => {
                self.compile_expr(target, code);
                code.push(Op::OptionalPropGet(prop.clone()));
            }
            Expr::Assign(name, rhs) => {
                self.compile_expr(rhs, code);
                code.push(Op::Store(name.clone()));
            }
            Expr::SetProp(target, prop, rhs) => {
                self.compile_expr(target, code);
                self.compile_expr(rhs, code);
                code.push(Op::PropSet(prop.clone()));
            }
            Expr::SetPropCompound(target, prop, op, rhs) => {
                self.compile_expr(target, code);
                self.compile_expr(rhs, code);
                code.push(Op::PropCompound(prop.clone(), *op));
            }
            Expr::SetIndex(target, index, rhs) => {
                self.compile_expr(target, code);
                self.compile_expr(index, code);
                self.compile_expr(rhs, code);
                code.push(Op::IndexSet);
            }
            Expr::UpdateVar(name, delta, postfix) => {
                code.push(Op::Load(name.clone()));
                if *postfix {
                    code.push(Op::Dup);
                    code.push(Op::Push(JsVal::Number(*delta)));
                    code.push(Op::Binary(BinOp::Add));
                    code.push(Op::Store(name.clone()));
                    code.push(Op::Pop);
                } else {
                    code.push(Op::Push(JsVal::Number(*delta)));
                    code.push(Op::Binary(BinOp::Add));
                    code.push(Op::Store(name.clone()));
                }
            }
            Expr::Unary(op, inner) => {
                self.compile_expr(inner, code);
                code.push(Op::Unary(*op));
            }
            Expr::Binary(op, lhs, rhs) => {
                self.compile_expr(lhs, code);
                self.compile_expr(rhs, code);
                code.push(Op::Binary(*op));
            }
            Expr::Nullish(lhs, rhs) => {
                self.compile_expr(lhs, code);
                let j_not_nullish = code.len();
                code.push(Op::JumpIfNullish(0));
                let j_end = code.len();
                code.push(Op::Jump(0));
                code[j_not_nullish] = Op::JumpIfNullish(code.len());
                code.push(Op::Pop);
                self.compile_expr(rhs, code);
                code[j_end] = Op::Jump(code.len());
            }
            Expr::Ternary(cond, then_e, else_e) => {
                self.compile_expr(cond, code);
                let j_false = code.len();
                code.push(Op::JumpIfFalse(0));
                self.compile_expr(then_e, code);
                let j_end = code.len();
                code.push(Op::Jump(0));
                code[j_false] = Op::JumpIfFalse(code.len());
                self.compile_expr(else_e, code);
                code[j_end] = Op::Jump(code.len());
            }
            Expr::Call(name, args) => {
                let spreads = self.compile_arg_elems(args, code);
                code.push(Op::Call(name.clone(), spreads));
            }
            Expr::CallMethod(target, method, args) => {
                let var_name = if let Expr::Var(v) = target.as_ref() {
                    Some(v.clone())
                } else {
                    None
                };
                self.compile_expr(target, code);
                let spreads = self.compile_arg_elems(args, code);
                code.push(Op::CallMethod(method.clone(), spreads, var_name));
            }
            Expr::OptionalCallMethod(target, method, args) => {
                self.compile_expr(target, code);
                let spreads = self.compile_arg_elems(args, code);
                code.push(Op::OptionalCallMethod(method.clone(), spreads));
            }
            Expr::CallValue(callee, args) => {
                self.compile_expr(callee, code);
                let spreads = self.compile_arg_elems(args, code);
                code.push(Op::CallValue(spreads));
            }
            Expr::New(ctor, args) => {
                let spreads = self.compile_arg_elems(args, code);
                code.push(Op::Construct(ctor.clone(), spreads));
            }
            Expr::Lambda(params, body) => {
                let id = self.compile_fn(params, body);
                code.push(Op::Push(JsVal::Function(id)));
            }
        }
    }
}

/// Estado mutable de la VM reutilizable entre múltiples `<script>` de una página.
pub struct VmRuntime {
    functions: Vec<FunctionChunk>,
    fn_names: HashMap<String, usize>,
    classes: HashMap<String, ClassDef>,
    pub globals: HashMap<String, JsVal>,
    pub objects: Vec<HashMap<String, JsVal>>,
    maps: Vec<Vec<(String, JsVal)>>,
    sets: Vec<Vec<JsVal>>,
    bytes_bufs: Vec<Vec<u8>>,
    steps: usize,
    depth: usize,
}

impl Default for VmRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl VmRuntime {
    pub fn new() -> Self {
        let mut vm = Self {
            functions: Vec::new(),
            fn_names: HashMap::new(),
            classes: HashMap::new(),
            globals: HashMap::new(),
            objects: Vec::new(),
            maps: Vec::new(),
            sets: Vec::new(),
            bytes_bufs: Vec::new(),
            steps: 0,
            depth: 0,
        };
        vm.globals
            .insert("Math".to_owned(), JsVal::NativeHandle("Math".to_owned()));
        vm.globals.insert(
            "Object".to_owned(),
            JsVal::NativeHandle("Object".to_owned()),
        );
        vm.globals
            .insert("Date".to_owned(), JsVal::NativeHandle("Date".to_owned()));
        vm
    }

    pub fn alloc_object(&mut self, map: HashMap<String, JsVal>) -> JsVal {
        let id = self.objects.len();
        self.objects.push(map);
        JsVal::Object(id)
    }

    pub fn alloc_uint8_array(&mut self, data: Vec<u8>) -> JsVal {
        let idx = self.bytes_bufs.len();
        self.bytes_bufs.push(data);
        self.alloc_object(HashMap::from([
            ("__wispType".to_owned(), JsVal::Str("Uint8Array".to_owned())),
            ("__wispIdx".to_owned(), JsVal::Number(idx as f64)),
        ]))
    }

    pub fn read_uint8_array(&self, val: &JsVal) -> Option<&[u8]> {
        let JsVal::Object(id) = val else {
            return None;
        };
        let map = self.objects.get(*id)?;
        if map.get("__wispType") != Some(&JsVal::Str("Uint8Array".to_owned())) {
            return None;
        }
        let JsVal::Number(idx) = map.get("__wispIdx")? else {
            return None;
        };
        self.bytes_bufs.get(*idx as usize).map(Vec::as_slice)
    }

    pub fn write_uint8_array(&mut self, val: &JsVal, data: Vec<u8>) -> bool {
        let JsVal::Object(id) = val else {
            return false;
        };
        let Some(map) = self.objects.get(*id) else {
            return false;
        };
        if map.get("__wispType") != Some(&JsVal::Str("Uint8Array".to_owned())) {
            return false;
        }
        let Some(JsVal::Number(idx)) = map.get("__wispIdx") else {
            return false;
        };
        if let Some(slot) = self.bytes_bufs.get_mut(*idx as usize) {
            *slot = data;
            return true;
        }
        false
    }

    pub fn eval_script(&mut self, src: &str, host: &mut dyn HostBridge) -> Result<JsVal, String> {
        let toks = tokenize(src)?;
        let stmts = Parser::new(toks).parse_program()?;
        let base_fn_id = self.functions.len();
        let mut compiler = Compiler::new();
        let mut main_code = compiler.compile_program(&stmts);
        if base_fn_id > 0 {
            remap_fn_ids(&mut main_code, base_fn_id);
            for chunk in &mut compiler.functions {
                remap_fn_ids(&mut chunk.code, base_fn_id);
            }
        }
        self.functions.extend(compiler.functions);
        for (name, id) in compiler.fn_names {
            self.fn_names.insert(name, id + base_fn_id);
        }

        let mut locals = HashMap::new();
        self.exec_code(&main_code, host, &mut locals)
            .map_err(|thrown| thrown.to_string())
    }

    pub fn invoke_fn(
        &mut self,
        fn_id: usize,
        args: Vec<JsVal>,
        host: &mut dyn HostBridge,
    ) -> Result<JsVal, String> {
        self.call_fn_id(fn_id, args, host)
            .map_err(|thrown| thrown.to_string())
    }

    fn call_fn_id_with_this(
        &mut self,
        fn_id: usize,
        this_val: Option<JsVal>,
        args: Vec<JsVal>,
        host: &mut dyn HostBridge,
    ) -> Result<JsVal, JsVal> {
        if self.depth >= MAX_RECURSION_DEPTH {
            return Err(JsVal::Str("límite de recursión excedido".into()));
        }
        let Some(chunk) = self.functions.get(fn_id).cloned() else {
            return Err(JsVal::Str(format!("función #{fn_id} no encontrada")));
        };
        let mut fn_locals = HashMap::new();
        if let Some(this_v) = this_val {
            fn_locals.insert("this".to_owned(), this_v);
        } else {
            fn_locals.insert("this".to_owned(), JsVal::Undefined);
        }
        for (i, param) in chunk.params.iter().enumerate() {
            if let Some(rest_name) = param.strip_prefix("...") {
                let rest_items = if i < args.len() {
                    args[i..].to_vec()
                } else {
                    Vec::new()
                };
                fn_locals.insert(rest_name.to_owned(), JsVal::Array(rest_items));
                break;
            }
            let val = args.get(i).cloned().unwrap_or(JsVal::Undefined);
            fn_locals.insert(param.clone(), val);
        }
        self.depth += 1;
        let res = self.exec_code(&chunk.code, host, &mut fn_locals);
        self.depth -= 1;
        res
    }

    fn call_fn_id(
        &mut self,
        fn_id: usize,
        args: Vec<JsVal>,
        host: &mut dyn HostBridge,
    ) -> Result<JsVal, JsVal> {
        self.call_fn_id_with_this(fn_id, None, args, host)
    }

    fn bind_pattern(
        &mut self,
        pat: &Pattern,
        val: JsVal,
        locals: &mut HashMap<String, JsVal>,
        host: &mut dyn HostBridge,
    ) -> Result<(), JsVal> {
        match pat {
            Pattern::Ident(name) => {
                let in_locals = locals.contains_key(name);
                let new_local = !locals.is_empty() && !self.globals.contains_key(name);
                if in_locals || new_local {
                    locals.insert(name.clone(), val);
                } else {
                    self.globals.insert(name.clone(), val);
                }
            }
            Pattern::Array(items, rest) => {
                let arr = match val {
                    JsVal::Array(a) => a,
                    _ => Vec::new(),
                };
                for (i, sub) in items.iter().enumerate() {
                    let item = arr.get(i).cloned().unwrap_or(JsVal::Undefined);
                    self.bind_pattern(sub, item, locals, host)?;
                }
                if let Some(rest_name) = rest {
                    let tail = if items.len() < arr.len() {
                        arr[items.len()..].to_vec()
                    } else {
                        Vec::new()
                    };
                    self.bind_pattern(
                        &Pattern::Ident(rest_name.clone()),
                        JsVal::Array(tail),
                        locals,
                        host,
                    )?;
                }
            }
            Pattern::Object(props) => {
                for (key, bind_name, default_expr) in props {
                    let mut extracted = match &val {
                        JsVal::Object(id) => self
                            .objects
                            .get(*id)
                            .and_then(|m| m.get(key).cloned())
                            .unwrap_or(JsVal::Undefined),
                        _ => host.get_prop(self, &val, key)?,
                    };
                    if matches!(extracted, JsVal::Undefined)
                        && let Some(def_e) = default_expr
                    {
                        let mut comp = Compiler::new();
                        let mut code = Vec::new();
                        comp.compile_expr(def_e, &mut code);
                        let base = self.functions.len();
                        remap_fn_ids(&mut code, base);
                        self.functions.extend(comp.functions);
                        extracted = self.exec_code(&code, host, locals)?;
                    }
                    self.bind_pattern(&Pattern::Ident(bind_name.clone()), extracted, locals, host)?;
                }
            }
        }
        Ok(())
    }

    fn collect_spread_args(&self, raw: Vec<JsVal>, spreads: &[bool]) -> Vec<JsVal> {
        let mut out = Vec::new();
        for (val, is_spread) in raw.into_iter().zip(spreads.iter().copied()) {
            if is_spread {
                match val {
                    JsVal::Array(items) => out.extend(items),
                    JsVal::Str(s) => out.extend(s.chars().map(|c| JsVal::Str(c.to_string()))),
                    other => out.push(other),
                }
            } else {
                out.push(val);
            }
        }
        out
    }

    fn exec_code(
        &mut self,
        code: &[Op],
        host: &mut dyn HostBridge,
        locals: &mut HashMap<String, JsVal>,
    ) -> Result<JsVal, JsVal> {
        let mut stack: Vec<JsVal> = Vec::new();
        let mut try_stack: Vec<(usize, usize)> = Vec::new();
        let mut ip = 0usize;

        while ip < code.len() {
            self.steps += 1;
            if self.steps > MAX_VM_STEPS {
                return Err(JsVal::Str(
                    "presupuesto de instrucciones de wisp-js agotado".into(),
                ));
            }
            let step_res: Result<Option<usize>, JsVal> = (|| {
                match &code[ip] {
                    Op::Push(v) => stack.push(v.clone()),
                    Op::Pop => {
                        let _ = stack.pop();
                    }
                    Op::Dup => {
                        let v = stack.last().cloned().unwrap_or(JsVal::Undefined);
                        stack.push(v);
                    }
                    Op::Load(name) => {
                        let val = locals
                            .get(name)
                            .or_else(|| self.globals.get(name))
                            .cloned()
                            .unwrap_or(JsVal::Undefined);
                        stack.push(val);
                    }
                    Op::Store(name) => {
                        let val = stack.last().cloned().unwrap_or(JsVal::Undefined);
                        let in_locals = locals.contains_key(name);
                        let new_local = !locals.is_empty() && !self.globals.contains_key(name);
                        if in_locals || new_local {
                            locals.insert(name.clone(), val);
                        } else {
                            self.globals.insert(name.clone(), val);
                        }
                    }
                    Op::BindPattern(pat) => {
                        let val = stack.last().cloned().unwrap_or(JsVal::Undefined);
                        self.bind_pattern(pat, val, locals, host)?;
                    }
                    Op::BindVar(name) => {
                        let val = stack.pop().unwrap_or(JsVal::Undefined);
                        if !locals.is_empty() && !self.globals.contains_key(name) {
                            locals.insert(name.clone(), val);
                        } else {
                            self.globals.insert(name.clone(), val);
                        }
                    }
                    Op::MakeRegex(pat, flags) => {
                        let obj = self.alloc_regex(pat, flags);
                        stack.push(obj);
                    }
                    Op::MakeArray(n) => {
                        let split = stack.len().saturating_sub(*n);
                        let items = stack.split_off(split);
                        stack.push(JsVal::Array(items));
                    }
                    Op::MakeArraySpread(spreads) => {
                        let split = stack.len().saturating_sub(spreads.len());
                        let raw = stack.split_off(split);
                        let items = self.collect_spread_args(raw, spreads);
                        stack.push(JsVal::Array(items));
                    }
                    Op::MakeObjectSpread(spec) => {
                        let split = stack.len().saturating_sub(spec.len());
                        let raw = stack.split_off(split);
                        let mut map = HashMap::new();
                        for (val, (key, is_spread)) in raw.into_iter().zip(spec.iter()) {
                            if *is_spread {
                                if let JsVal::Object(id) = val
                                    && let Some(src) = self.objects.get(id)
                                {
                                    for (k, v) in src {
                                        if !k.starts_with("__wisp") {
                                            map.insert(k.clone(), v.clone());
                                        }
                                    }
                                }
                            } else if let Some(k) = key {
                                map.insert(k.clone(), val);
                            }
                        }
                        let obj = self.alloc_object(map);
                        stack.push(obj);
                    }
                    Op::IterKeys => {
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let keys: Vec<JsVal> = match target {
                            JsVal::Object(id) => self
                                .objects
                                .get(id)
                                .map(|m| {
                                    let mut ks: Vec<String> = m
                                        .keys()
                                        .filter(|k| !k.starts_with("__wisp"))
                                        .cloned()
                                        .collect();
                                    ks.sort();
                                    ks.into_iter().map(JsVal::Str).collect()
                                })
                                .unwrap_or_default(),
                            JsVal::Array(items) => {
                                (0..items.len()).map(|i| JsVal::Str(i.to_string())).collect()
                            }
                            _ => Vec::new(),
                        };
                        stack.push(JsVal::Array(keys));
                    }
                    Op::IndexGet => {
                        let idx = stack.pop().unwrap_or(JsVal::Undefined);
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let val = match (&target, &idx) {
                            (JsVal::Array(items), JsVal::Number(n)) if *n >= 0.0 => {
                                items.get(*n as usize).cloned().unwrap_or(JsVal::Undefined)
                            }
                            (JsVal::Str(s), JsVal::Number(n)) if *n >= 0.0 => s
                                .chars()
                                .nth(*n as usize)
                                .map(|c| JsVal::Str(c.to_string()))
                                .unwrap_or(JsVal::Undefined),
                            (JsVal::Object(id), key) => {
                                if let Some(bytes) = self.read_uint8_array(&target)
                                    && let JsVal::Number(n) = key
                                    && *n >= 0.0
                                {
                                    bytes
                                        .get(*n as usize)
                                        .map(|b| JsVal::Number(*b as f64))
                                        .unwrap_or(JsVal::Undefined)
                                } else {
                                    self.objects
                                        .get(*id)
                                        .and_then(|m| m.get(&key.to_string()).cloned())
                                        .unwrap_or(JsVal::Undefined)
                                }
                            }
                            (other, key) => host.get_prop(self, other, &key.to_string())?,
                        };
                        stack.push(val);
                    }
                    Op::IndexSet => {
                        let val = stack.pop().unwrap_or(JsVal::Undefined);
                        let idx = stack.pop().unwrap_or(JsVal::Undefined);
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        if let JsVal::Object(id) = target {
                            if let Some(map) = self.objects.get(&id)
                                && map.get("__wispType")
                                    == Some(&JsVal::Str("Uint8Array".to_owned()))
                                && let Some(JsVal::Number(bidx)) = map.get("__wispIdx")
                                && let JsVal::Number(pos) = idx
                                && pos >= 0.0
                                && let Some(buf) = self.bytes_bufs.get_mut(*bidx as usize)
                                && let Some(slot) = buf.get_mut(pos as usize)
                            {
                                *slot = (val.to_number() as i64 & 0xff) as u8;
                            } else if let Some(map) = self.objects.get_mut(id) {
                                map.insert(idx.to_string(), val.clone());
                            }
                        }
                        stack.push(val);
                    }
                    Op::PropGet(prop) => {
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let val = self.get_prop_inner(&target, prop, host)?;
                        stack.push(val);
                    }
                    Op::OptionalPropGet(prop) => {
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let val = if matches!(target, JsVal::Null | JsVal::Undefined) {
                            JsVal::Undefined
                        } else {
                            self.get_prop_inner(&target, prop, host)?
                        };
                        stack.push(val);
                    }
                    Op::PropSet(prop) => {
                        let val = stack.pop().unwrap_or(JsVal::Undefined);
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        match &target {
                            JsVal::Object(id) => {
                                if let Some(map) = self.objects.get_mut(*id) {
                                    map.insert(prop.clone(), val.clone());
                                }
                            }
                            _ => host.set_prop(self, &target, prop, val.clone())?,
                        }
                        stack.push(val);
                    }
                    Op::PropCompound(prop, op) => {
                        let rhs = stack.pop().unwrap_or(JsVal::Undefined);
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let cur = self.get_prop_inner(&target, prop, host)?;
                        let next = eval_binary(*op, cur, rhs);
                        match &target {
                            JsVal::Object(id) => {
                                if let Some(map) = self.objects.get_mut(*id) {
                                    map.insert(prop.clone(), next.clone());
                                }
                            }
                            _ => host.set_prop(self, &target, prop, next.clone())?,
                        }
                        stack.push(next);
                    }
                    Op::Unary(op) => {
                        let v = stack.pop().unwrap_or(JsVal::Undefined);
                        stack.push(match op {
                            UnaryOp::Neg => JsVal::Number(-v.to_number()),
                            UnaryOp::Not => JsVal::Bool(!v.is_truthy()),
                            UnaryOp::Void => JsVal::Undefined,
                            UnaryOp::Typeof => JsVal::Str(
                                match v {
                                    JsVal::Undefined => "undefined",
                                    JsVal::Bool(_) => "boolean",
                                    JsVal::Number(_) => "number",
                                    JsVal::Str(_) => "string",
                                    JsVal::Function(_) => "function",
                                    JsVal::Null
                                    | JsVal::Array(_)
                                    | JsVal::Object(_)
                                    | JsVal::DomNode(_)
                                    | JsVal::NativeHandle(_) => "object",
                                }
                                .to_owned(),
                            ),
                        });
                    }
                    Op::Binary(op) => {
                        let rhs = stack.pop().unwrap_or(JsVal::Undefined);
                        let lhs = stack.pop().unwrap_or(JsVal::Undefined);
                        if matches!(op, BinOp::In) {
                            let key = lhs.to_string();
                            let found = match rhs {
                                JsVal::Object(id) => self
                                    .objects
                                    .get(id)
                                    .is_some_and(|m| m.contains_key(&key)),
                                JsVal::Array(items) => {
                                    key.parse::<usize>().is_ok_and(|i| i < items.len())
                                }
                                _ => false,
                            };
                            stack.push(JsVal::Bool(found));
                        } else {
                            stack.push(eval_binary(*op, lhs, rhs));
                        }
                    }
                    Op::Jump(target) => return Ok(Some(*target)),
                    Op::JumpIfFalse(target) => {
                        let cond = stack.pop().unwrap_or(JsVal::Undefined);
                        if !cond.is_truthy() {
                            return Ok(Some(*target));
                        }
                    }
                    Op::JumpIfNullish(target) => {
                        let v = stack.last().unwrap_or(&JsVal::Undefined);
                        if matches!(v, JsVal::Null | JsVal::Undefined) {
                            return Ok(Some(*target));
                        }
                    }
                    Op::TryStart(catch_ip) => {
                        try_stack.push((*catch_ip, stack.len()));
                    }
                    Op::TryEnd => {
                        let _ = try_stack.pop();
                    }
                    Op::Throw => {
                        let err = stack.pop().unwrap_or(JsVal::Undefined);
                        return Err(err);
                    }
                    Op::Call(name, spreads) => {
                        let split = stack.len().saturating_sub(spreads.len());
                        let raw = stack.split_off(split);
                        let args = self.collect_spread_args(raw, spreads);
                        let local_fn = locals.get(name).or_else(|| self.globals.get(name)).cloned();
                        let ret = if let Some(JsVal::Function(fn_id)) = local_fn {
                            self.call_fn_id(fn_id, args, host)?
                        } else if let Some(&fn_id) = self.fn_names.get(name) {
                            self.call_fn_id(fn_id, args, host)?
                        } else if name == "super" {
                            if let Some(JsVal::Str(parent_name)) = locals
                                .get("this")
                                .and_then(|v| match v {
                                    JsVal::Object(id) => self.objects.get(*id)?.get("__wispParent"),
                                    _ => None,
                                })
                                .cloned()
                                && let Some(parent_ctor) =
                                    self.classes.get(&parent_name).and_then(|c| c.ctor)
                            {
                                let this_val = locals.get("this").cloned();
                                self.call_fn_id_with_this(parent_ctor, this_val, args, host)?;
                            }
                            JsVal::Undefined
                        } else if let Some(res) = self.call_builtin_global(name, &args) {
                            res
                        } else if let Some(res) = host.call_global(self, name, args) {
                            res?
                        } else {
                            return Err(JsVal::Str(format!("función no definida `{name}`")));
                        };
                        stack.push(ret);
                    }
                    Op::CallMethod(method, spreads, var_name) => {
                        let split = stack.len().saturating_sub(spreads.len());
                        let raw = stack.split_off(split);
                        let args = self.collect_spread_args(raw, spreads);
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let mut updated_arr: Option<Vec<JsVal>> = None;
                        let ret = if let JsVal::Array(mut items) = target.clone() {
                            if let Some(res) = self.dispatch_array_mut(&mut items, method, &args) {
                                updated_arr = Some(items);
                                res
                            } else {
                                self.dispatch_method(target, method, args, host)?
                            }
                        } else {
                            self.dispatch_method(target, method, args, host)?
                        };
                        if let (Some(new_items), Some(vname)) = (updated_arr, var_name) {
                            let new_val = JsVal::Array(new_items);
                            if locals.contains_key(vname) {
                                locals.insert(vname.clone(), new_val);
                            } else {
                                self.globals.insert(vname.clone(), new_val);
                            }
                        }
                        stack.push(ret);
                    }
                    Op::OptionalCallMethod(method, spreads) => {
                        let split = stack.len().saturating_sub(spreads.len());
                        let raw = stack.split_off(split);
                        let args = self.collect_spread_args(raw, spreads);
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let ret = if matches!(target, JsVal::Null | JsVal::Undefined) {
                            JsVal::Undefined
                        } else {
                            self.dispatch_method(target, method, args, host)?
                        };
                        stack.push(ret);
                    }
                    Op::CallValue(spreads) => {
                        let split = stack.len().saturating_sub(spreads.len());
                        let raw = stack.split_off(split);
                        let args = self.collect_spread_args(raw, spreads);
                        let callee = stack.pop().unwrap_or(JsVal::Undefined);
                        match callee {
                            JsVal::Function(fn_id) => {
                                let ret = self.call_fn_id(fn_id, args, host)?;
                                stack.push(ret);
                            }
                            _ => {
                                return Err(JsVal::Str("el valor no es invocable".into()));
                            }
                        }
                    }
                    Op::Construct(ctor, spreads) => {
                        let split = stack.len().saturating_sub(spreads.len());
                        let raw = stack.split_off(split);
                        let args = self.collect_spread_args(raw, spreads);
                        let obj = if let Some(def) = self.classes.get(ctor).cloned() {
                            self.construct_class(ctor, &def, args, host)?
                        } else if let Some(obj) = self.construct_builtin(ctor, &args) {
                            obj?
                        } else {
                            host.construct(self, ctor, args)?
                        };
                        stack.push(obj);
                    }
                    Op::RegisterClass {
                        name,
                        parent,
                        ctor,
                        methods,
                    } => {
                        let mut mmap = HashMap::new();
                        if let Some(p) = parent
                            && let Some(pdef) = self.classes.get(p)
                        {
                            mmap.extend(pdef.methods.clone());
                        }
                        for (mname, mid) in methods {
                            mmap.insert(mname.clone(), *mid);
                        }
                        self.classes.insert(
                            name.clone(),
                            ClassDef {
                                parent: parent.clone(),
                                ctor: *ctor,
                                methods: mmap,
                            },
                        );
                        stack.push(JsVal::Undefined);
                    }
                    Op::Return => {
                        return Ok(Some(usize::MAX));
                    }
                }
                Ok(None)
            })();

            match step_res {
                Ok(Some(usize::MAX)) => return Ok(stack.pop().unwrap_or(JsVal::Undefined)),
                Ok(Some(next_ip)) => ip = next_ip,
                Ok(None) => ip += 1,
                Err(thrown) => {
                    if let Some((catch_ip, stack_len)) = try_stack.pop() {
                        stack.truncate(stack_len);
                        stack.push(thrown);
                        ip = catch_ip;
                    } else {
                        return Err(thrown);
                    }
                }
            }
        }

        Ok(stack.pop().unwrap_or(JsVal::Undefined))
    }

    fn construct_class(
        &mut self,
        _name: &str,
        def: &ClassDef,
        args: Vec<JsVal>,
        host: &mut dyn HostBridge,
    ) -> Result<JsVal, JsVal> {
        let mut map = HashMap::new();
        if let Some(p) = &def.parent {
            map.insert("__wispParent".to_owned(), JsVal::Str(p.clone()));
        }
        for (mname, mid) in &def.methods {
            map.insert(mname.clone(), JsVal::Function(*mid));
        }
        let instance = self.alloc_object(map);
        if let Some(ctor_id) = def.ctor {
            self.call_fn_id_with_this(ctor_id, Some(instance.clone()), args, host)?;
        }
        Ok(instance)
    }

    fn alloc_regex(&mut self, pat: &str, flags: &str) -> JsVal {
        self.alloc_object(HashMap::from([
            ("__wispType".to_owned(), JsVal::Str("RegExp".to_owned())),
            ("source".to_owned(), JsVal::Str(pat.to_owned())),
            ("flags".to_owned(), JsVal::Str(flags.to_owned())),
            ("ignoreCase".to_owned(), JsVal::Bool(flags.contains('i'))),
            ("global".to_owned(), JsVal::Bool(flags.contains('g'))),
        ]))
    }

    fn construct_builtin(&mut self, ctor: &str, args: &[JsVal]) -> Option<Result<JsVal, JsVal>> {
        match ctor {
            "Map" => {
                let idx = self.maps.len();
                let mut entries = Vec::new();
                if let Some(JsVal::Array(pairs)) = args.first() {
                    for p in pairs {
                        if let JsVal::Array(kv) = p
                            && kv.len() >= 2
                        {
                            entries.push((kv[0].to_string(), kv[1].clone()));
                        }
                    }
                }
                self.maps.push(entries);
                Some(Ok(self.alloc_object(HashMap::from([
                    ("__wispType".to_owned(), JsVal::Str("Map".to_owned())),
                    ("__wispIdx".to_owned(), JsVal::Number(idx as f64)),
                ]))))
            }
            "Set" => {
                let idx = self.sets.len();
                let mut items = Vec::new();
                if let Some(JsVal::Array(init)) = args.first() {
                    for x in init {
                        if !items.contains(x) {
                            items.push(x.clone());
                        }
                    }
                }
                self.sets.push(items);
                Some(Ok(self.alloc_object(HashMap::from([
                    ("__wispType".to_owned(), JsVal::Str("Set".to_owned())),
                    ("__wispIdx".to_owned(), JsVal::Number(idx as f64)),
                ]))))
            }
            "RegExp" => {
                let pat = args.first().map(ToString::to_string).unwrap_or_default();
                let flags = args.get(1).map(ToString::to_string).unwrap_or_default();
                Some(Ok(self.alloc_regex(&pat, &flags)))
            }
            "URLSearchParams" => {
                let raw = args.first().map(ToString::to_string).unwrap_or_default();
                let qs = raw.strip_prefix('?').unwrap_or(&raw);
                let mut pairs = Vec::new();
                for part in qs.split('&').filter(|s| !s.is_empty()) {
                    let (k, v) = part.split_once('=').unwrap_or((part, ""));
                    pairs.push((pct_decode(k), JsVal::Str(pct_decode(v))));
                }
                let idx = self.maps.len();
                self.maps.push(pairs);
                Some(Ok(self.alloc_object(HashMap::from([
                    ("__wispType".to_owned(), JsVal::Str("URLSearchParams".to_owned())),
                    ("__wispIdx".to_owned(), JsVal::Number(idx as f64)),
                ]))))
            }
            "Uint8Array" => {
                let data = match args.first() {
                    Some(JsVal::Number(n)) if *n > 0.0 => vec![0u8; (*n as usize).min(1_048_576)],
                    Some(JsVal::Array(items)) => items
                        .iter()
                        .map(|v| (v.to_number() as i64 & 0xff) as u8)
                        .collect(),
                    _ => Vec::new(),
                };
                Some(Ok(self.alloc_uint8_array(data)))
            }
            "Date" => {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0);
                Some(Ok(self.alloc_object(HashMap::from([
                    ("__wispType".to_owned(), JsVal::Str("Date".to_owned())),
                    ("__wispTime".to_owned(), JsVal::Number(ms)),
                ]))))
            }
            _ => None,
        }
    }

    fn call_builtin_global(&self, name: &str, args: &[JsVal]) -> Option<JsVal> {
        match name {
            "String" => Some(JsVal::Str(
                args.first().map(ToString::to_string).unwrap_or_default(),
            )),
            "Number" => Some(JsVal::Number(args.first().map_or(0.0, JsVal::to_number))),
            "Boolean" => Some(JsVal::Bool(args.first().is_some_and(JsVal::is_truthy))),
            "parseInt" => {
                let s = args.first().map(ToString::to_string).unwrap_or_default();
                let radix = args
                    .get(1)
                    .map(|v| v.to_number() as u32)
                    .filter(|r| (2..=36).contains(r))
                    .unwrap_or(10);
                let trimmed = s.trim();
                let parsed = i64::from_str_radix(trimmed, radix)
                    .map(|n| n as f64)
                    .unwrap_or(f64::NAN);
                Some(JsVal::Number(parsed))
            }
            "parseFloat" => {
                let s = args.first().map(ToString::to_string).unwrap_or_default();
                Some(JsVal::Number(
                    s.trim().parse::<f64>().unwrap_or(f64::NAN),
                ))
            }
            "isNaN" => Some(JsVal::Bool(
                args.first().is_none_or(|v| v.to_number().is_nan()),
            )),
            "isFinite" => Some(JsVal::Bool(
                args.first().is_some_and(|v| v.to_number().is_finite()),
            )),
            "encodeURIComponent" => {
                let s = args.first().map(ToString::to_string).unwrap_or_default();
                Some(JsVal::Str(pct_encode(&s)))
            }
            "decodeURIComponent" => {
                let s = args.first().map(ToString::to_string).unwrap_or_default();
                Some(JsVal::Str(pct_decode(&s)))
            }
            "btoa" => {
                let s = args.first().map(ToString::to_string).unwrap_or_default();
                Some(JsVal::Str(b64_encode(s.as_bytes())))
            }
            "atob" => {
                let s = args.first().map(ToString::to_string).unwrap_or_default();
                let bytes = b64_decode(&s).unwrap_or_default();
                Some(JsVal::Str(String::from_utf8_lossy(&bytes).into_owned()))
            }
            _ => None,
        }
    }

    fn get_prop_inner(
        &mut self,
        target: &JsVal,
        prop: &str,
        host: &mut dyn HostBridge,
    ) -> Result<JsVal, JsVal> {
        match target {
            JsVal::Array(items) if prop == "length" => Ok(JsVal::Number(items.len() as f64)),
            JsVal::Str(s) if prop == "length" => {
                Ok(JsVal::Number(s.encode_utf16().count() as f64))
            }
            JsVal::NativeHandle(h) if h == "Math" => Ok(match prop {
                "PI" => JsVal::Number(std::f64::consts::PI),
                "E" => JsVal::Number(std::f64::consts::E),
                _ => JsVal::Undefined,
            }),
            JsVal::Object(id) => {
                let map = self.objects.get(*id);
                if let Some(m) = map {
                    if let Some(JsVal::Str(wtype)) = m.get("__wispType") {
                        match (wtype.as_str(), prop) {
                            ("Map", "size") => {
                                let idx = m
                                    .get("__wispIdx")
                                    .and_then(|v| match v {
                                        JsVal::Number(n) => Some(*n as usize),
                                        _ => None,
                                    })
                                    .unwrap_or(0);
                                let len = self.maps.get(idx).map_or(0, Vec::len);
                                return Ok(JsVal::Number(len as f64));
                            }
                            ("Set", "size") => {
                                let idx = m
                                    .get("__wispIdx")
                                    .and_then(|v| match v {
                                        JsVal::Number(n) => Some(*n as usize),
                                        _ => None,
                                    })
                                    .unwrap_or(0);
                                let len = self.sets.get(idx).map_or(0, Vec::len);
                                return Ok(JsVal::Number(len as f64));
                            }
                            ("Uint8Array", "length" | "byteLength") => {
                                let idx = m
                                    .get("__wispIdx")
                                    .and_then(|v| match v {
                                        JsVal::Number(n) => Some(*n as usize),
                                        _ => None,
                                    })
                                    .unwrap_or(0);
                                let len = self.bytes_bufs.get(idx).map_or(0, Vec::len);
                                return Ok(JsVal::Number(len as f64));
                            }
                            _ => {}
                        }
                    }
                    if let Some(v) = m.get(prop) {
                        return Ok(v.clone());
                    }
                }
                Ok(JsVal::Undefined)
            }
            _ => host.get_prop(self, target, prop),
        }
    }

    fn dispatch_array_mut(
        &mut self,
        items: &mut Vec<JsVal>,
        method: &str,
        args: &[JsVal],
    ) -> Option<JsVal> {
        match method {
            "push" => {
                items.extend(args.iter().cloned());
                Some(JsVal::Number(items.len() as f64))
            }
            "pop" => Some(items.pop().unwrap_or(JsVal::Undefined)),
            "shift" => {
                if items.is_empty() {
                    Some(JsVal::Undefined)
                } else {
                    Some(items.remove(0))
                }
            }
            "unshift" => {
                for (i, arg) in args.iter().enumerate() {
                    items.insert(i, arg.clone());
                }
                Some(JsVal::Number(items.len() as f64))
            }
            "reverse" => {
                items.reverse();
                Some(JsVal::Array(items.clone()))
            }
            "sort" => {
                items.sort_by_key(ToString::to_string);
                Some(JsVal::Array(items.clone()))
            }
            _ => None,
        }
    }

    fn dispatch_method(
        &mut self,
        target: JsVal,
        method: &str,
        args: Vec<JsVal>,
        host: &mut dyn HostBridge,
    ) -> Result<JsVal, JsVal> {
        match &target {
            JsVal::NativeHandle(h) if h == "Math" => {
                let n0 = args.first().map_or(f64::NAN, JsVal::to_number);
                let n1 = args.get(1).map_or(f64::NAN, JsVal::to_number);
                return Ok(match method {
                    "floor" => JsVal::Number(n0.floor()),
                    "ceil" => JsVal::Number(n0.ceil()),
                    "round" => JsVal::Number(n0.round()),
                    "abs" => JsVal::Number(n0.abs()),
                    "sqrt" => JsVal::Number(n0.sqrt()),
                    "pow" => JsVal::Number(n0.powf(n1)),
                    "min" => JsVal::Number(
                        args.iter()
                            .map(JsVal::to_number)
                            .fold(f64::INFINITY, f64::min),
                    ),
                    "max" => JsVal::Number(
                        args.iter()
                            .map(JsVal::to_number)
                            .fold(f64::NEG_INFINITY, f64::max),
                    ),
                    "random" => {
                        let nanos = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.subsec_nanos())
                            .unwrap_or(123456);
                        JsVal::Number(((nanos as f64) % 1_000_000.0) / 1_000_000.0)
                    }
                    _ => JsVal::Undefined,
                });
            }
            JsVal::NativeHandle(h) if h == "Object" => match method {
                "keys" => {
                    if let Some(JsVal::Object(id)) = args.first()
                        && let Some(m) = self.objects.get(*id)
                    {
                        let mut keys: Vec<String> = m
                            .keys()
                            .filter(|k| !k.starts_with("__wisp"))
                            .cloned()
                            .collect();
                        keys.sort();
                        return Ok(JsVal::Array(keys.into_iter().map(JsVal::Str).collect()));
                    }
                    return Ok(JsVal::Array(Vec::new()));
                }
                "values" => {
                    if let Some(JsVal::Object(id)) = args.first()
                        && let Some(m) = self.objects.get(*id)
                    {
                        let mut pairs: Vec<(&String, &JsVal)> =
                            m.iter().filter(|(k, _)| !k.starts_with("__wisp")).collect();
                        pairs.sort_by_key(|(k, _)| *k);
                        return Ok(JsVal::Array(
                            pairs.into_iter().map(|(_, v)| v.clone()).collect(),
                        ));
                    }
                    return Ok(JsVal::Array(Vec::new()));
                }
                "entries" => {
                    if let Some(JsVal::Object(id)) = args.first()
                        && let Some(m) = self.objects.get(*id)
                    {
                        let mut pairs: Vec<(String, JsVal)> = m
                            .iter()
                            .filter(|(k, _)| !k.starts_with("__wisp"))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        pairs.sort_by(|a, b| a.0.cmp(&b.0));
                        return Ok(JsVal::Array(
                            pairs
                                .into_iter()
                                .map(|(k, v)| JsVal::Array(vec![JsVal::Str(k), v]))
                                .collect(),
                        ));
                    }
                    return Ok(JsVal::Array(Vec::new()));
                }
                "assign" => {
                    if let Some(JsVal::Object(dst_id)) = args.first().cloned() {
                        for src in args.iter().skip(1) {
                            if let JsVal::Object(src_id) = src
                                && let Some(src_map) = self.objects.get(*src_id).cloned()
                                && let Some(dst_map) = self.objects.get_mut(dst_id)
                            {
                                for (k, v) in src_map {
                                    if !k.starts_with("__wisp") {
                                        dst_map.insert(k, v);
                                    }
                                }
                            }
                        }
                        return Ok(JsVal::Object(dst_id));
                    }
                    return Ok(JsVal::Undefined);
                }
                "freeze" => return Ok(args.first().cloned().unwrap_or(JsVal::Undefined)),
                _ => {}
            },
            JsVal::NativeHandle(h) if h == "Date" && method == "now" => {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0);
                return Ok(JsVal::Number(ms));
            }
            JsVal::Array(items) => match method {
                "join" => {
                    let sep = args
                        .first()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| ",".to_owned());
                    let joined: Vec<String> = items.iter().map(ToString::to_string).collect();
                    return Ok(JsVal::Str(joined.join(&sep)));
                }
                "includes" => {
                    let needle = args.first().cloned().unwrap_or(JsVal::Undefined);
                    return Ok(JsVal::Bool(items.contains(&needle)));
                }
                "indexOf" => {
                    let needle = args.first().cloned().unwrap_or(JsVal::Undefined);
                    let pos = items
                        .iter()
                        .position(|x| x == &needle)
                        .map(|i| i as f64)
                        .unwrap_or(-1.0);
                    return Ok(JsVal::Number(pos));
                }
                "slice" => {
                    let len = items.len() as isize;
                    let start = norm_slice_idx(
                        args.first().map_or(0, |v| v.to_number() as isize),
                        len,
                    );
                    let end = norm_slice_idx(
                        args.get(1).map_or(len, |v| v.to_number() as isize),
                        len,
                    );
                    let sliced = if start < end {
                        items[start..end].to_vec()
                    } else {
                        Vec::new()
                    };
                    return Ok(JsVal::Array(sliced));
                }
                "concat" => {
                    let mut out = items.clone();
                    for a in args {
                        match a {
                            JsVal::Array(more) => out.extend(more),
                            other => out.push(other),
                        }
                    }
                    return Ok(JsVal::Array(out));
                }
                "flat" => {
                    let mut out = Vec::new();
                    for item in items {
                        match item {
                            JsVal::Array(sub) => out.extend(sub.iter().cloned()),
                            other => out.push(other.clone()),
                        }
                    }
                    return Ok(JsVal::Array(out));
                }
                "map" => {
                    if let Some(JsVal::Function(fn_id)) = args.first().copied() {
                        let mut out = Vec::with_capacity(items.len());
                        for (i, item) in items.iter().enumerate() {
                            out.push(self.call_fn_id(
                                fn_id,
                                vec![item.clone(), JsVal::Number(i as f64)],
                                host,
                            )?);
                        }
                        return Ok(JsVal::Array(out));
                    }
                }
                "filter" => {
                    if let Some(JsVal::Function(fn_id)) = args.first().copied() {
                        let mut out = Vec::new();
                        for (i, item) in items.iter().enumerate() {
                            let keep = self.call_fn_id(
                                fn_id,
                                vec![item.clone(), JsVal::Number(i as f64)],
                                host,
                            )?;
                            if keep.is_truthy() {
                                out.push(item.clone());
                            }
                        }
                        return Ok(JsVal::Array(out));
                    }
                }
                "forEach" => {
                    if let Some(JsVal::Function(fn_id)) = args.first().copied() {
                        for (i, item) in items.iter().enumerate() {
                            self.call_fn_id(
                                fn_id,
                                vec![item.clone(), JsVal::Number(i as f64)],
                                host,
                            )?;
                        }
                    }
                    return Ok(JsVal::Undefined);
                }
                "reduce" => {
                    if let Some(JsVal::Function(fn_id)) = args.first().copied() {
                        let mut iter = items.iter().enumerate();
                        let mut acc = if let Some(init) = args.get(1) {
                            init.clone()
                        } else if let Some((_, first)) = iter.next() {
                            first.clone()
                        } else {
                            return Err(JsVal::Str(
                                "Reduce of empty array with no initial value".into(),
                            ));
                        };
                        for (i, item) in iter {
                            acc = self.call_fn_id(
                                fn_id,
                                vec![acc, item.clone(), JsVal::Number(i as f64)],
                                host,
                            )?;
                        }
                        return Ok(acc);
                    }
                }
                "find" => {
                    if let Some(JsVal::Function(fn_id)) = args.first().copied() {
                        for (i, item) in items.iter().enumerate() {
                            if self
                                .call_fn_id(
                                    fn_id,
                                    vec![item.clone(), JsVal::Number(i as f64)],
                                    host,
                                )?
                                .is_truthy()
                            {
                                return Ok(item.clone());
                            }
                        }
                    }
                    return Ok(JsVal::Undefined);
                }
                "findIndex" => {
                    if let Some(JsVal::Function(fn_id)) = args.first().copied() {
                        for (i, item) in items.iter().enumerate() {
                            if self
                                .call_fn_id(
                                    fn_id,
                                    vec![item.clone(), JsVal::Number(i as f64)],
                                    host,
                                )?
                                .is_truthy()
                            {
                                return Ok(JsVal::Number(i as f64));
                            }
                        }
                    }
                    return Ok(JsVal::Number(-1.0));
                }
                "some" => {
                    if let Some(JsVal::Function(fn_id)) = args.first().copied() {
                        for (i, item) in items.iter().enumerate() {
                            if self
                                .call_fn_id(
                                    fn_id,
                                    vec![item.clone(), JsVal::Number(i as f64)],
                                    host,
                                )?
                                .is_truthy()
                            {
                                return Ok(JsVal::Bool(true));
                            }
                        }
                    }
                    return Ok(JsVal::Bool(false));
                }
                "every" => {
                    if let Some(JsVal::Function(fn_id)) = args.first().copied() {
                        for (i, item) in items.iter().enumerate() {
                            if !self
                                .call_fn_id(
                                    fn_id,
                                    vec![item.clone(), JsVal::Number(i as f64)],
                                    host,
                                )?
                                .is_truthy()
                            {
                                return Ok(JsVal::Bool(false));
                            }
                        }
                    }
                    return Ok(JsVal::Bool(true));
                }
                _ => {}
            },
            JsVal::Str(s) => match method {
                "trim" => return Ok(JsVal::Str(s.trim().to_owned())),
                "toLowerCase" => return Ok(JsVal::Str(s.to_ascii_lowercase())),
                "toUpperCase" => return Ok(JsVal::Str(s.to_ascii_uppercase())),
                "includes" => {
                    let sub = args.first().map(ToString::to_string).unwrap_or_default();
                    return Ok(JsVal::Bool(s.contains(&sub)));
                }
                "startsWith" => {
                    let sub = args.first().map(ToString::to_string).unwrap_or_default();
                    return Ok(JsVal::Bool(s.starts_with(&sub)));
                }
                "endsWith" => {
                    let sub = args.first().map(ToString::to_string).unwrap_or_default();
                    return Ok(JsVal::Bool(s.ends_with(&sub)));
                }
                "indexOf" => {
                    let sub = args.first().map(ToString::to_string).unwrap_or_default();
                    let pos = s.find(&sub).map(|i| i as f64).unwrap_or(-1.0);
                    return Ok(JsVal::Number(pos));
                }
                "slice" | "substring" => {
                    let chars: Vec<char> = s.chars().collect();
                    let len = chars.len() as isize;
                    let start = norm_slice_idx(
                        args.first().map_or(0, |v| v.to_number() as isize),
                        len,
                    );
                    let end = norm_slice_idx(
                        args.get(1).map_or(len, |v| v.to_number() as isize),
                        len,
                    );
                    let out: String = if start < end {
                        chars[start..end].iter().collect()
                    } else {
                        String::new()
                    };
                    return Ok(JsVal::Str(out));
                }
                "charAt" => {
                    let idx = args.first().map_or(0, |v| v.to_number().max(0.0) as usize);
                    return Ok(JsVal::Str(
                        s.chars().nth(idx).map(|c| c.to_string()).unwrap_or_default(),
                    ));
                }
                "split" => {
                    let sep = args.first().map(ToString::to_string).unwrap_or_default();
                    let parts = if sep.is_empty() {
                        s.chars().map(|c| JsVal::Str(c.to_string())).collect()
                    } else {
                        s.split(&sep).map(|p| JsVal::Str(p.to_owned())).collect()
                    };
                    return Ok(JsVal::Array(parts));
                }
                "replace" => {
                    let rep = args.get(1).map(ToString::to_string).unwrap_or_default();
                    if let Some(JsVal::Object(rid)) = args.first()
                        && let Some(rmap) = self.objects.get(*rid)
                        && rmap.get("__wispType") == Some(&JsVal::Str("RegExp".to_owned()))
                    {
                        let pat = rmap.get("source").map(ToString::to_string).unwrap_or_default();
                        let ci = rmap.get("ignoreCase") == Some(&JsVal::Bool(true));
                        let global = rmap.get("global") == Some(&JsVal::Bool(true));
                        return Ok(JsVal::Str(regex_replace(s, &pat, &rep, ci, global)));
                    }
                    let pat = args.first().map(ToString::to_string).unwrap_or_default();
                    return Ok(JsVal::Str(s.replacen(&pat, &rep, 1)));
                }
                "replaceAll" => {
                    let pat = args.first().map(ToString::to_string).unwrap_or_default();
                    let rep = args.get(1).map(ToString::to_string).unwrap_or_default();
                    return Ok(JsVal::Str(s.replace(&pat, &rep)));
                }
                "match" => {
                    if let Some(JsVal::Object(rid)) = args.first()
                        && let Some(rmap) = self.objects.get(*rid)
                        && rmap.get("__wispType") == Some(&JsVal::Str("RegExp".to_owned()))
                    {
                        let pat = rmap.get("source").map(ToString::to_string).unwrap_or_default();
                        let ci = rmap.get("ignoreCase") == Some(&JsVal::Bool(true));
                        let global = rmap.get("global") == Some(&JsVal::Bool(true));
                        let matches = regex_find_all(s, &pat, ci, global);
                        return Ok(if matches.is_empty() {
                            JsVal::Null
                        } else {
                            JsVal::Array(matches.into_iter().map(JsVal::Str).collect())
                        });
                    }
                    let pat = args.first().map(ToString::to_string).unwrap_or_default();
                    let matches = regex_find_all(s, &pat, false, false);
                    return Ok(if matches.is_empty() {
                        JsVal::Null
                    } else {
                        JsVal::Array(matches.into_iter().map(JsVal::Str).collect())
                    });
                }
                _ => {}
            },
            JsVal::Object(id) => {
                let map_clone = self.objects.get(*id).cloned();
                if let Some(m) = map_clone {
                    if let Some(JsVal::Str(wtype)) = m.get("__wispType") {
                        let idx = m
                            .get("__wispIdx")
                            .and_then(|v| match v {
                                JsVal::Number(n) => Some(*n as usize),
                                _ => None,
                            })
                            .unwrap_or(0);
                        match wtype.as_str() {
                            "Map" => match method {
                                "set" => {
                                    let k =
                                        args.first().map(ToString::to_string).unwrap_or_default();
                                    let v = args.get(1).cloned().unwrap_or(JsVal::Undefined);
                                    if let Some(entries) = self.maps.get_mut(idx) {
                                        if let Some(slot) =
                                            entries.iter_mut().find(|(ek, _)| ek == &k)
                                        {
                                            slot.1 = v;
                                        } else {
                                            entries.push((k, v));
                                        }
                                    }
                                    return Ok(target);
                                }
                                "get" => {
                                    let k =
                                        args.first().map(ToString::to_string).unwrap_or_default();
                                    let found = self
                                        .maps
                                        .get(idx)
                                        .and_then(|e| e.iter().find(|(ek, _)| ek == &k))
                                        .map(|(_, v)| v.clone())
                                        .unwrap_or(JsVal::Undefined);
                                    return Ok(found);
                                }
                                "has" => {
                                    let k =
                                        args.first().map(ToString::to_string).unwrap_or_default();
                                    let has = self
                                        .maps
                                        .get(idx)
                                        .is_some_and(|e| e.iter().any(|(ek, _)| ek == &k));
                                    return Ok(JsVal::Bool(has));
                                }
                                "delete" => {
                                    let k =
                                        args.first().map(ToString::to_string).unwrap_or_default();
                                    if let Some(entries) = self.maps.get_mut(idx) {
                                        let before = entries.len();
                                        entries.retain(|(ek, _)| ek != &k);
                                        return Ok(JsVal::Bool(entries.len() < before));
                                    }
                                    return Ok(JsVal::Bool(false));
                                }
                                "keys" => {
                                    let ks = self
                                        .maps
                                        .get(idx)
                                        .map(|e| {
                                            e.iter().map(|(k, _)| JsVal::Str(k.clone())).collect()
                                        })
                                        .unwrap_or_default();
                                    return Ok(JsVal::Array(ks));
                                }
                                "values" => {
                                    let vs = self
                                        .maps
                                        .get(idx)
                                        .map(|e| e.iter().map(|(_, v)| v.clone()).collect())
                                        .unwrap_or_default();
                                    return Ok(JsVal::Array(vs));
                                }
                                _ => {}
                            },
                            "Set" => match method {
                                "add" => {
                                    let v = args.first().cloned().unwrap_or(JsVal::Undefined);
                                    if let Some(items) = self.sets.get_mut(idx)
                                        && !items.contains(&v)
                                    {
                                        items.push(v);
                                    }
                                    return Ok(target);
                                }
                                "has" => {
                                    let v = args.first().cloned().unwrap_or(JsVal::Undefined);
                                    let has = self.sets.get(idx).is_some_and(|s| s.contains(&v));
                                    return Ok(JsVal::Bool(has));
                                }
                                "delete" => {
                                    let v = args.first().cloned().unwrap_or(JsVal::Undefined);
                                    if let Some(items) = self.sets.get_mut(idx) {
                                        let before = items.len();
                                        items.retain(|x| x != &v);
                                        return Ok(JsVal::Bool(items.len() < before));
                                    }
                                    return Ok(JsVal::Bool(false));
                                }
                                "values" => {
                                    let vs = self.sets.get(idx).cloned().unwrap_or_default();
                                    return Ok(JsVal::Array(vs));
                                }
                                _ => {}
                            },
                            "RegExp" => {
                                let pat =
                                    m.get("source").map(ToString::to_string).unwrap_or_default();
                                let ci = m.get("ignoreCase") == Some(&JsVal::Bool(true));
                                let text =
                                    args.first().map(ToString::to_string).unwrap_or_default();
                                match method {
                                    "test" => {
                                        return Ok(JsVal::Bool(
                                            !regex_find_all(&text, &pat, ci, false).is_empty(),
                                        ));
                                    }
                                    "exec" => {
                                        let found = regex_find_all(&text, &pat, ci, false);
                                        return Ok(if found.is_empty() {
                                            JsVal::Null
                                        } else {
                                            JsVal::Array(
                                                found.into_iter().map(JsVal::Str).collect(),
                                            )
                                        });
                                    }
                                    _ => {}
                                }
                            }
                            "URLSearchParams" => match method {
                                "get" => {
                                    let k =
                                        args.first().map(ToString::to_string).unwrap_or_default();
                                    let found = self
                                        .maps
                                        .get(idx)
                                        .and_then(|e| e.iter().find(|(ek, _)| ek == &k))
                                        .map(|(_, v)| v.clone())
                                        .unwrap_or(JsVal::Null);
                                    return Ok(found);
                                }
                                "has" => {
                                    let k =
                                        args.first().map(ToString::to_string).unwrap_or_default();
                                    let has = self
                                        .maps
                                        .get(idx)
                                        .is_some_and(|e| e.iter().any(|(ek, _)| ek == &k));
                                    return Ok(JsVal::Bool(has));
                                }
                                "set" => {
                                    let k =
                                        args.first().map(ToString::to_string).unwrap_or_default();
                                    let v = JsVal::Str(
                                        args.get(1).map(ToString::to_string).unwrap_or_default(),
                                    );
                                    if let Some(entries) = self.maps.get_mut(idx) {
                                        entries.retain(|(ek, _)| ek != &k);
                                        entries.push((k, v));
                                    }
                                    return Ok(JsVal::Undefined);
                                }
                                "append" => {
                                    let k =
                                        args.first().map(ToString::to_string).unwrap_or_default();
                                    let v = JsVal::Str(
                                        args.get(1).map(ToString::to_string).unwrap_or_default(),
                                    );
                                    if let Some(entries) = self.maps.get_mut(idx) {
                                        entries.push((k, v));
                                    }
                                    return Ok(JsVal::Undefined);
                                }
                                "delete" => {
                                    let k =
                                        args.first().map(ToString::to_string).unwrap_or_default();
                                    if let Some(entries) = self.maps.get_mut(idx) {
                                        entries.retain(|(ek, _)| ek != &k);
                                    }
                                    return Ok(JsVal::Undefined);
                                }
                                "toString" => {
                                    let qs = self
                                        .maps
                                        .get(idx)
                                        .map(|e| {
                                            e.iter()
                                                .map(|(k, v)| {
                                                    format!(
                                                        "{}={}",
                                                        pct_encode(k),
                                                        pct_encode(&v.to_string())
                                                    )
                                                })
                                                .collect::<Vec<_>>()
                                                .join("&")
                                        })
                                        .unwrap_or_default();
                                    return Ok(JsVal::Str(qs));
                                }
                                _ => {}
                            },
                            "Date" if method == "getTime" => {
                                return Ok(m
                                    .get("__wispTime")
                                    .cloned()
                                    .unwrap_or(JsVal::Number(0.0)));
                            }
                            _ => {}
                        }
                    }
                    if let Some(JsVal::Function(fn_id)) = m.get(method).copied() {
                        return self.call_fn_id_with_this(fn_id, Some(target), args, host);
                    }
                }
            }
            _ => {}
        }
        host.call_method(self, &target, method, args)
    }
}

fn norm_slice_idx(idx: isize, len: isize) -> usize {
    if idx < 0 {
        (len + idx).max(0) as usize
    } else {
        idx.min(len) as usize
    }
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
        {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(val) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            )
        {
            out.push(val);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn b64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        out.push(TABLE[b0 >> 2] as char);
        out.push(TABLE[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[b2 & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::new();
    for b in s.bytes() {
        if b == b'=' || b.is_ascii_whitespace() {
            continue;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        vals.push(v);
    }
    let mut out = Vec::new();
    for chunk in vals.chunks(4) {
        if chunk.len() >= 2 {
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
        }
        if chunk.len() >= 3 {
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        if chunk.len() == 4 {
            out.push((chunk[2] << 6) | chunk[3]);
        }
    }
    Some(out)
}

#[derive(Clone)]
enum ReAtom {
    Any,
    Lit(char),
    Digit,
    Word,
    Space,
    Class(bool, Vec<(char, char)>),
}

#[derive(Clone)]
struct RePiece {
    atom: ReAtom,
    min: usize,
    max: Option<usize>,
}

fn parse_re_branch(pat: &str) -> Vec<RePiece> {
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < chars.len() {
        let atom = match chars[i] {
            '.' => {
                i += 1;
                ReAtom::Any
            }
            '\\' if i + 1 < chars.len() => {
                i += 1;
                let a = match chars[i] {
                    'd' => ReAtom::Digit,
                    'w' => ReAtom::Word,
                    's' => ReAtom::Space,
                    c => ReAtom::Lit(c),
                };
                i += 1;
                a
            }
            '[' => {
                i += 1;
                let neg = chars.get(i) == Some(&'^');
                if neg {
                    i += 1;
                }
                let mut ranges = Vec::new();
                while i < chars.len() && chars[i] != ']' {
                    let start = chars[i];
                    if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != ']' {
                        ranges.push((start, chars[i + 2]));
                        i += 3;
                    } else {
                        ranges.push((start, start));
                        i += 1;
                    }
                }
                if i < chars.len() && chars[i] == ']' {
                    i += 1;
                }
                ReAtom::Class(neg, ranges)
            }
            c => {
                i += 1;
                ReAtom::Lit(c)
            }
        };
        let (min, max) = match chars.get(i) {
            Some('*') => {
                i += 1;
                (0, None)
            }
            Some('+') => {
                i += 1;
                (1, None)
            }
            Some('?') => {
                i += 1;
                (0, Some(1))
            }
            _ => (1, Some(1)),
        };
        out.push(RePiece { atom, min, max });
    }
    out
}

fn atom_matches(atom: &ReAtom, ch: char, ci: bool) -> bool {
    let eq = |a: char, b: char| {
        if ci {
            a.eq_ignore_ascii_case(&b)
        } else {
            a == b
        }
    };
    match atom {
        ReAtom::Any => ch != '\n',
        ReAtom::Lit(c) => eq(*c, ch),
        ReAtom::Digit => ch.is_ascii_digit(),
        ReAtom::Word => ch.is_ascii_alphanumeric() || ch == '_',
        ReAtom::Space => ch.is_whitespace(),
        ReAtom::Class(neg, ranges) => {
            let hit = ranges.iter().any(|(lo, hi)| {
                if ci {
                    let c = ch.to_ascii_lowercase();
                    c >= lo.to_ascii_lowercase() && c <= hi.to_ascii_lowercase()
                } else {
                    ch >= *lo && ch <= *hi
                }
            });
            if *neg { !hit } else { hit }
        }
    }
}

fn match_pieces_at(pieces: &[RePiece], chars: &[char], pos: usize, ci: bool) -> Option<usize> {
    if pieces.is_empty() {
        return Some(pos);
    }
    let first = &pieces[0];
    let rest = &pieces[1..];
    let mut max_take = 0usize;
    while first.max.is_none_or(|m| max_take < m)
        && pos + max_take < chars.len()
        && atom_matches(&first.atom, chars[pos + max_take], ci)
    {
        max_take += 1;
    }
    for take in (first.min..=max_take).rev() {
        if let Some(end) = match_pieces_at(rest, chars, pos + take, ci) {
            return Some(end);
        }
    }
    None
}

fn regex_match_branch(branch: &str, chars: &[char], start: usize, ci: bool) -> Option<usize> {
    let anchor_start = branch.starts_with('^');
    let anchor_end = branch.ends_with('$');
    if anchor_start && start != 0 {
        return None;
    }
    let trimmed = branch
        .strip_prefix('^')
        .unwrap_or(branch)
        .strip_suffix('$')
        .unwrap_or(branch.strip_prefix('^').unwrap_or(branch));
    let pieces = parse_re_branch(trimmed);
    let end = match_pieces_at(&pieces, chars, start, ci)?;
    if anchor_end && end != chars.len() {
        return None;
    }
    Some(end)
}

fn regex_find_all(text: &str, pat: &str, ci: bool, global: bool) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let branches: Vec<&str> = pat.split('|').collect();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos <= chars.len() {
        let mut best: Option<usize> = None;
        for b in &branches {
            if let Some(end) = regex_match_branch(b, &chars, pos, ci) {
                best = Some(end);
                break;
            }
        }
        if let Some(end) = best {
            out.push(chars[pos..end].iter().collect());
            if !global {
                break;
            }
            pos = if end > pos { end } else { pos + 1 };
        } else {
            pos += 1;
        }
    }
    out
}

fn regex_replace(text: &str, pat: &str, rep: &str, ci: bool, global: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let branches: Vec<&str> = pat.split('|').collect();
    let mut out = String::new();
    let mut pos = 0usize;
    while pos < chars.len() {
        let mut matched = None;
        for b in &branches {
            if let Some(end) = regex_match_branch(b, &chars, pos, ci) {
                matched = Some(end);
                break;
            }
        }
        if let Some(end) = matched {
            out.push_str(rep);
            pos = if end > pos { end } else { pos + 1 };
            if !global {
                out.extend(chars[pos..].iter());
                break;
            }
        } else {
            out.push(chars[pos]);
            pos += 1;
        }
    }
    out
}

fn tok_as_prop_name(tok: Tok) -> Option<String> {
    match tok {
        Tok::Ident(s) => Some(s),
        Tok::KwLet => Some("let".to_owned()),
        Tok::KwConst => Some("const".to_owned()),
        Tok::KwVar => Some("var".to_owned()),
        Tok::KwFunction => Some("function".to_owned()),
        Tok::KwClass => Some("class".to_owned()),
        Tok::KwExtends => Some("extends".to_owned()),
        Tok::KwThis => Some("this".to_owned()),
        Tok::KwSuper => Some("super".to_owned()),
        Tok::KwOf => Some("of".to_owned()),
        Tok::KwIn => Some("in".to_owned()),
        Tok::KwReturn => Some("return".to_owned()),
        Tok::KwIf => Some("if".to_owned()),
        Tok::KwElse => Some("else".to_owned()),
        Tok::KwWhile => Some("while".to_owned()),
        Tok::KwFor => Some("for".to_owned()),
        Tok::KwSwitch => Some("switch".to_owned()),
        Tok::KwCase => Some("case".to_owned()),
        Tok::KwDefault => Some("default".to_owned()),
        Tok::KwBreak => Some("break".to_owned()),
        Tok::KwContinue => Some("continue".to_owned()),
        Tok::KwTry => Some("try".to_owned()),
        Tok::KwCatch => Some("catch".to_owned()),
        Tok::KwFinally => Some("finally".to_owned()),
        Tok::KwThrow => Some("throw".to_owned()),
        Tok::KwNew => Some("new".to_owned()),
        Tok::KwTypeof => Some("typeof".to_owned()),
        Tok::KwVoid => Some("void".to_owned()),
        Tok::KwAsync => Some("async".to_owned()),
        Tok::KwAwait => Some("await".to_owned()),
        Tok::KwTrue => Some("true".to_owned()),
        Tok::KwFalse => Some("false".to_owned()),
        Tok::KwNull => Some("null".to_owned()),
        Tok::KwUndefined => Some("undefined".to_owned()),
        _ => None,
    }
}

fn remap_fn_ids(code: &mut [Op], offset: usize) {
    for op in code {
        match op {
            Op::Push(JsVal::Function(id)) => *id += offset,
            Op::RegisterClass { ctor, methods, .. } => {
                if let Some(c) = ctor {
                    *c += offset;
                }
                for (_, m) in methods {
                    *m += offset;
                }
            }
            _ => {}
        }
    }
}

/// Evalúa un fragmento JavaScript con el motor propio `wisp-js::vm`.
pub fn eval_native(src: &str) -> Result<JsVal, String> {
    let mut vm = VmRuntime::new();
    let mut host = NoopHost;
    vm.eval_script(src, &mut host)
}

fn eval_binary(op: BinOp, lhs: JsVal, rhs: JsVal) -> JsVal {
    match op {
        BinOp::Add => match (&lhs, &rhs) {
            (JsVal::Str(a), _) => JsVal::Str(format!("{a}{rhs}")),
            (_, JsVal::Str(b)) => JsVal::Str(format!("{lhs}{b}")),
            _ => JsVal::Number(lhs.to_number() + rhs.to_number()),
        },
        BinOp::Sub => JsVal::Number(lhs.to_number() - rhs.to_number()),
        BinOp::Mul => JsVal::Number(lhs.to_number() * rhs.to_number()),
        BinOp::Div => JsVal::Number(lhs.to_number() / rhs.to_number()),
        BinOp::Rem => JsVal::Number(lhs.to_number() % rhs.to_number()),
        BinOp::StrictEq | BinOp::Eq => JsVal::Bool(lhs == rhs),
        BinOp::StrictNe | BinOp::Ne => JsVal::Bool(lhs != rhs),
        BinOp::Lt => JsVal::Bool(lhs.to_number() < rhs.to_number()),
        BinOp::Le => JsVal::Bool(lhs.to_number() <= rhs.to_number()),
        BinOp::Gt => JsVal::Bool(lhs.to_number() > rhs.to_number()),
        BinOp::Ge => JsVal::Bool(lhs.to_number() >= rhs.to_number()),
        BinOp::In => JsVal::Bool(false),
        BinOp::And => {
            if lhs.is_truthy() {
                rhs
            } else {
                lhs
            }
        }
        BinOp::Or => {
            if lhs.is_truthy() {
                lhs
            } else {
                rhs
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executes_arithmetic_variables_loops_and_functions() {
        let res = eval_native(
            r#"
            function sumTo(n) {
                let acc = 0;
                for (let i = 1; i <= n; i++) {
                    acc += i;
                }
                return acc;
            }
            sumTo(10);
            "#,
        )
        .unwrap();
        assert_eq!(res, JsVal::Number(55.0));
    }

    #[test]
    fn enforces_step_budget_against_infinite_loops() {
        let err = eval_native("while (true) {}").unwrap_err();
        assert!(err.contains("presupuesto"));
    }

    #[test]
    fn supports_arrays_strings_objects_and_try_catch() {
        let res = eval_native(
            r#"
            const xs = [10, 20, 30];
            const obj = { label: "ok" };
            let caught = "";
            try {
                throw "err";
            } catch (e) {
                caught = e;
            }
            obj.label + "-" + xs[2] + "-" + caught;
            "#,
        )
        .unwrap();
        assert_eq!(res, JsVal::Str("ok-30-err".into()));
    }

    #[test]
    fn supports_classes_destructuring_spread_template_and_collections() {
        let res = eval_native(
            r#"
            class Box {
                constructor(v) { this.v = v; }
                scale(k) { return this.v * k; }
            }
            class NamedBox extends Box {
                constructor(name, v) {
                    super(v);
                    this.name = name;
                }
                label() { return `${this.name}:${this.scale(2)}`; }
            }
            const nb = new NamedBox("wisp", 21);
            const { a, b = 5 } = { a: 10 };
            const arr = [1, 2, ...[3, 4]];
            arr.push(5);
            const doubled = arr.filter(x => x > 2).map(x => x * 2);
            const m = new Map();
            m.set("k", doubled.reduce((acc, x) => acc + x, 0));
            const s = new Set([1, 1, 2]);
            const re = /^[a-z]+:\d+$/i;
            const qs = new URLSearchParams("foo=bar&n=7");
            `${nb.label()}|${a + b}|${m.get("k")}|${s.size}|${re.test(nb.label())}|${qs.get("foo")}`;
            "#,
        )
        .unwrap();
        assert_eq!(res, JsVal::Str("wisp:42|15|24|2|true|bar".into()));
    }
}
