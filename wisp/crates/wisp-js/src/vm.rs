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
    KwReturn,
    KwIf,
    KwElse,
    KwWhile,
    KwFor,
    KwBreak,
    KwContinue,
    KwTry,
    KwCatch,
    KwThrow,
    KwNew,
    KwTypeof,
    KwVoid,
    KwTrue,
    KwFalse,
    KwNull,
    KwUndefined,
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
            out.push(Tok::Str(s));
            continue;
        }
        if b.is_ascii_digit() {
            let start = i;
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
                "return" => Tok::KwReturn,
                "if" => Tok::KwIf,
                "else" => Tok::KwElse,
                "while" => Tok::KwWhile,
                "for" => Tok::KwFor,
                "break" => Tok::KwBreak,
                "continue" => Tok::KwContinue,
                "try" => Tok::KwTry,
                "catch" => Tok::KwCatch,
                "throw" => Tok::KwThrow,
                "new" => Tok::KwNew,
                "typeof" => Tok::KwTypeof,
                "void" => Tok::KwVoid,
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
enum Expr {
    Lit(JsVal),
    Var(String),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Index(Box<Expr>, Box<Expr>),
    Prop(Box<Expr>, String),
    Assign(String, Box<Expr>),
    SetProp(Box<Expr>, String, Box<Expr>),
    SetPropCompound(Box<Expr>, String, BinOp, Box<Expr>),
    SetIndex(Box<Expr>, Box<Expr>, Box<Expr>),
    UpdateVar(String, f64, bool),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    CallMethod(Box<Expr>, String, Vec<Expr>),
    CallValue(Box<Expr>, Vec<Expr>),
    New(String, Vec<Expr>),
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
    And,
    Or,
}

#[derive(Debug, Clone)]
enum Stmt {
    VarDecl(String, Option<Expr>),
    Block(Vec<Stmt>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    For(Option<Box<Stmt>>, Option<Expr>, Option<Expr>, Box<Stmt>),
    Break,
    Continue,
    TryCatch(Vec<Stmt>, Option<String>, Vec<Stmt>),
    Throw(Expr),
    FnDecl(String, Vec<String>, Vec<Stmt>),
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

    fn parse_var_decl(&mut self, consume_semi: bool) -> Result<Stmt, String> {
        self.next();
        let name = match self.next() {
            Tok::Ident(n) => n,
            other => {
                return Err(format!(
                    "se esperaba identificador tras declaración, no {other:?}"
                ));
            }
        };
        let init = if self.eat(&Tok::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        if consume_semi {
            let _ = self.eat(&Tok::Semi);
        }
        Ok(Stmt::VarDecl(name, init))
    }

    fn parse_stmt(&mut self) -> Result<Stmt, String> {
        while self.eat(&Tok::Semi) {}
        match self.peek().clone() {
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
                self.expect(&Tok::KwCatch)?;
                let catch_var = if self.eat(&Tok::LParen) {
                    let name = match self.next() {
                        Tok::Ident(n) => n,
                        other => return Err(format!("identificador catch inválido: {other:?}")),
                    };
                    self.expect(&Tok::RParen)?;
                    Some(name)
                } else {
                    None
                };
                let catch_body = self.parse_block_stmts()?;
                Ok(Stmt::TryCatch(try_body, catch_var, catch_body))
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
            if let Tok::Ident(p) = self.next() {
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
        let cond = self.parse_or()?;
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

    fn parse_arg_list(&mut self) -> Result<Vec<Expr>, String> {
        let mut args = Vec::new();
        while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
            args.push(self.parse_expr()?);
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
                    other => Expr::CallValue(Box::new(other), args),
                };
            } else if self.eat(&Tok::Dot) {
                let prop = match self.next() {
                    Tok::Ident(p) => p,
                    other => return Err(format!("propiedad inválida tras `.`: {other:?}")),
                };
                expr = Expr::Prop(Box::new(expr), prop);
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

    fn parse_primary(&mut self) -> Result<Expr, String> {
        if self.is_arrow_params() {
            let params = self.parse_param_list()?;
            let body = self.parse_arrow_body()?;
            return Ok(Expr::Lambda(params, body));
        }
        match self.next() {
            Tok::Num(n) => Ok(Expr::Lit(JsVal::Number(n))),
            Tok::Str(s) => Ok(Expr::Lit(JsVal::Str(s))),
            Tok::KwTrue => Ok(Expr::Lit(JsVal::Bool(true))),
            Tok::KwFalse => Ok(Expr::Lit(JsVal::Bool(false))),
            Tok::KwNull => Ok(Expr::Lit(JsVal::Null)),
            Tok::KwUndefined => Ok(Expr::Lit(JsVal::Undefined)),
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
                    items.push(self.parse_expr()?);
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
                    let key = match self.next() {
                        Tok::Ident(k) | Tok::Str(k) => k,
                        other => return Err(format!("clave de objeto inválida: {other:?}")),
                    };
                    if self.eat(&Tok::Colon) {
                        let val = self.parse_expr()?;
                        pairs.push((key, val));
                    } else {
                        pairs.push((key.clone(), Expr::Var(key)));
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
    MakeArray(usize),
    MakeObject(usize),
    IndexGet,
    IndexSet,
    PropGet(String),
    PropSet(String),
    PropCompound(String, BinOp),
    Unary(UnaryOp),
    Binary(BinOp),
    Jump(usize),
    JumpIfFalse(usize),
    TryStart(usize),
    TryEnd,
    Throw,
    BindVar(String),
    Call(String, usize),
    CallMethod(String, usize),
    CallValue(usize),
    Construct(String, usize),
    Return,
}

#[derive(Debug, Clone)]
struct FunctionChunk {
    params: Vec<String>,
    code: Vec<Op>,
}

struct LoopContext {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

struct Compiler {
    functions: Vec<FunctionChunk>,
    fn_names: HashMap<String, usize>,
    loops: Vec<LoopContext>,
}

impl Compiler {
    fn new() -> Self {
        Self {
            functions: Vec::new(),
            fn_names: HashMap::new(),
            loops: Vec::new(),
        }
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
            Stmt::VarDecl(name, init) => {
                if let Some(expr) = init {
                    self.compile_expr(expr, code);
                } else {
                    code.push(Op::Push(JsVal::Undefined));
                }
                code.push(Op::Store(name.clone()));
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
            Stmt::TryCatch(try_body, catch_var, catch_body) => {
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

    fn compile_expr(&mut self, expr: &Expr, code: &mut Vec<Op>) {
        match expr {
            Expr::Lit(val) => code.push(Op::Push(val.clone())),
            Expr::Var(name) => code.push(Op::Load(name.clone())),
            Expr::Array(items) => {
                for item in items {
                    self.compile_expr(item, code);
                }
                code.push(Op::MakeArray(items.len()));
            }
            Expr::Object(pairs) => {
                for (k, v) in pairs {
                    code.push(Op::Push(JsVal::Str(k.clone())));
                    self.compile_expr(v, code);
                }
                code.push(Op::MakeObject(pairs.len()));
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
                for arg in args {
                    self.compile_expr(arg, code);
                }
                code.push(Op::Call(name.clone(), args.len()));
            }
            Expr::CallMethod(target, method, args) => {
                self.compile_expr(target, code);
                for arg in args {
                    self.compile_expr(arg, code);
                }
                code.push(Op::CallMethod(method.clone(), args.len()));
            }
            Expr::CallValue(callee, args) => {
                self.compile_expr(callee, code);
                for arg in args {
                    self.compile_expr(arg, code);
                }
                code.push(Op::CallValue(args.len()));
            }
            Expr::New(ctor, args) => {
                for arg in args {
                    self.compile_expr(arg, code);
                }
                code.push(Op::Construct(ctor.clone(), args.len()));
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
    pub globals: HashMap<String, JsVal>,
    pub objects: Vec<HashMap<String, JsVal>>,
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
        Self {
            functions: Vec::new(),
            fn_names: HashMap::new(),
            globals: HashMap::new(),
            objects: Vec::new(),
            steps: 0,
            depth: 0,
        }
    }

    pub fn alloc_object(&mut self, map: HashMap<String, JsVal>) -> JsVal {
        let id = self.objects.len();
        self.objects.push(map);
        JsVal::Object(id)
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

    fn call_fn_id(
        &mut self,
        fn_id: usize,
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
        for (param, arg) in chunk.params.iter().zip(args) {
            fn_locals.insert(param.clone(), arg);
        }
        self.depth += 1;
        let res = self.exec_code(&chunk.code, host, &mut fn_locals);
        self.depth -= 1;
        res
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
                    Op::BindVar(name) => {
                        let val = stack.pop().unwrap_or(JsVal::Undefined);
                        if !locals.is_empty() && !self.globals.contains_key(name) {
                            locals.insert(name.clone(), val);
                        } else {
                            self.globals.insert(name.clone(), val);
                        }
                    }
                    Op::MakeArray(n) => {
                        let split = stack.len().saturating_sub(*n);
                        let items = stack.split_off(split);
                        stack.push(JsVal::Array(items));
                    }
                    Op::MakeObject(n) => {
                        let split = stack.len().saturating_sub(n * 2);
                        let raw = stack.split_off(split);
                        let mut map = HashMap::new();
                        for pair in raw.as_chunks::<2>().0 {
                            map.insert(pair[0].to_string(), pair[1].clone());
                        }
                        let obj = self.alloc_object(map);
                        stack.push(obj);
                    }
                    Op::IndexGet => {
                        let idx = stack.pop().unwrap_or(JsVal::Undefined);
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let val = match (target, idx) {
                            (JsVal::Array(items), JsVal::Number(n)) if n >= 0.0 => {
                                items.get(n as usize).cloned().unwrap_or(JsVal::Undefined)
                            }
                            (JsVal::Object(id), key) => self
                                .objects
                                .get(id)
                                .and_then(|m| m.get(&key.to_string()).cloned())
                                .unwrap_or(JsVal::Undefined),
                            (other, key) => host.get_prop(self, &other, &key.to_string())?,
                        };
                        stack.push(val);
                    }
                    Op::IndexSet => {
                        let val = stack.pop().unwrap_or(JsVal::Undefined);
                        let idx = stack.pop().unwrap_or(JsVal::Undefined);
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        if let JsVal::Object(id) = target
                            && let Some(map) = self.objects.get_mut(id)
                        {
                            map.insert(idx.to_string(), val.clone());
                        }
                        stack.push(val);
                    }
                    Op::PropGet(prop) => {
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let val = match &target {
                            JsVal::Array(items) if prop == "length" => {
                                JsVal::Number(items.len() as f64)
                            }
                            JsVal::Str(s) if prop == "length" => {
                                JsVal::Number(s.encode_utf16().count() as f64)
                            }
                            JsVal::Object(id) => self
                                .objects
                                .get(*id)
                                .and_then(|m| m.get(prop).cloned())
                                .unwrap_or(JsVal::Undefined),
                            _ => host.get_prop(self, &target, prop)?,
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
                        let cur = match &target {
                            JsVal::Object(id) => self
                                .objects
                                .get(*id)
                                .and_then(|m| m.get(prop).cloned())
                                .unwrap_or(JsVal::Undefined),
                            _ => host.get_prop(self, &target, prop)?,
                        };
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
                        stack.push(eval_binary(*op, lhs, rhs));
                    }
                    Op::Jump(target) => return Ok(Some(*target)),
                    Op::JumpIfFalse(target) => {
                        let cond = stack.pop().unwrap_or(JsVal::Undefined);
                        if !cond.is_truthy() {
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
                    Op::Call(name, argc) => {
                        let split = stack.len().saturating_sub(*argc);
                        let args = stack.split_off(split);
                        let local_fn = locals.get(name).or_else(|| self.globals.get(name)).cloned();
                        let ret = if let Some(JsVal::Function(fn_id)) = local_fn {
                            self.call_fn_id(fn_id, args, host)?
                        } else if let Some(&fn_id) = self.fn_names.get(name) {
                            self.call_fn_id(fn_id, args, host)?
                        } else if name == "String" {
                            JsVal::Str(args.first().map(ToString::to_string).unwrap_or_default())
                        } else if name == "Number" {
                            JsVal::Number(args.first().map_or(0.0, JsVal::to_number))
                        } else if name == "Boolean" {
                            JsVal::Bool(args.first().is_some_and(JsVal::is_truthy))
                        } else if let Some(res) = host.call_global(self, name, args) {
                            res?
                        } else {
                            return Err(JsVal::Str(format!("función no definida `{name}`")));
                        };
                        stack.push(ret);
                    }
                    Op::CallMethod(method, argc) => {
                        let split = stack.len().saturating_sub(*argc);
                        let args = stack.split_off(split);
                        let target = stack.pop().unwrap_or(JsVal::Undefined);
                        let ret = self.dispatch_method(target, method, args, host)?;
                        stack.push(ret);
                    }
                    Op::CallValue(argc) => {
                        let split = stack.len().saturating_sub(*argc);
                        let args = stack.split_off(split);
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
                    Op::Construct(ctor, argc) => {
                        let split = stack.len().saturating_sub(*argc);
                        let args = stack.split_off(split);
                        let obj = host.construct(self, ctor, args)?;
                        stack.push(obj);
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

    fn dispatch_method(
        &mut self,
        target: JsVal,
        method: &str,
        args: Vec<JsVal>,
        host: &mut dyn HostBridge,
    ) -> Result<JsVal, JsVal> {
        match &target {
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
                    let pat = args.first().map(ToString::to_string).unwrap_or_default();
                    let rep = args.get(1).map(ToString::to_string).unwrap_or_default();
                    return Ok(JsVal::Str(s.replacen(&pat, &rep, 1)));
                }
                _ => {}
            },
            JsVal::Object(id) => {
                if let Some(JsVal::Function(fn_id)) =
                    self.objects.get(*id).and_then(|m| m.get(method).cloned())
                {
                    return self.call_fn_id(fn_id, args, host);
                }
            }
            _ => {}
        }
        host.call_method(self, &target, method, args)
    }
}

fn remap_fn_ids(code: &mut [Op], offset: usize) {
    for op in code {
        if let Op::Push(JsVal::Function(id)) = op {
            *id += offset;
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
}
