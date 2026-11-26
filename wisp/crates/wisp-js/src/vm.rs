//! Motor JavaScript propio de Wisp (ítem 15 del backlog):
//! `lexer -> parser -> AST -> bytecode -> intérprete` (VM de pila).
//!
//! Independiente de Boa y V8: permite ejecutar expresiones, variables,
//! control de flujo, funciones, arrays y bucles con presupuesto determinista
//! de instrucciones.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;

const MAX_VM_STEPS: usize = 200_000;

#[derive(Debug, Clone, PartialEq)]
pub enum JsVal {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<JsVal>),
    Function(usize),
}

impl JsVal {
    pub fn is_truthy(&self) -> bool {
        match self {
            JsVal::Undefined | JsVal::Null => false,
            JsVal::Bool(b) => *b,
            JsVal::Number(n) => *n != 0.0 && !n.is_nan(),
            JsVal::Str(s) => !s.is_empty(),
            JsVal::Array(_) | JsVal::Function(_) => true,
        }
    }

    pub fn to_number(&self) -> f64 {
        match self {
            JsVal::Undefined => f64::NAN,
            JsVal::Null | JsVal::Bool(false) => 0.0,
            JsVal::Bool(true) => 1.0,
            JsVal::Number(n) => *n,
            JsVal::Str(s) => s.trim().parse::<f64>().unwrap_or(f64::NAN),
            JsVal::Array(_) | JsVal::Function(_) => f64::NAN,
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
                if n.fract() == 0.0 && n.abs() < 1e15 {
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
            JsVal::Function(idx) => write!(f, "[Function #{idx}]"),
        }
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
    KwTrue,
    KwFalse,
    KwNull,
    KwUndefined,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
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
        if b == b'"' || b == b'\'' {
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
    Index(Box<Expr>, Box<Expr>),
    Assign(String, Box<Expr>),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
}

#[derive(Debug, Clone, Copy)]
enum UnaryOp {
    Neg,
    Not,
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
            Err(format!("se esperaba `{expected:?}`, se encontró `{:?}`", self.peek()))
        }
    }

    fn parse_program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut stmts = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, String> {
        while self.eat(&Tok::Semi) {}
        match self.peek().clone() {
            Tok::KwLet | Tok::KwConst | Tok::KwVar => {
                self.next();
                let name = match self.next() {
                    Tok::Ident(n) => n,
                    other => return Err(format!("se esperaba identificador tras declaración, no {other:?}")),
                };
                let init = if self.eat(&Tok::Eq) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                let _ = self.eat(&Tok::Semi);
                Ok(Stmt::VarDecl(name, init))
            }
            Tok::KwFunction => {
                self.next();
                let name = match self.next() {
                    Tok::Ident(n) => n,
                    other => return Err(format!("nombre de función inválido: {other:?}")),
                };
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
                self.expect(&Tok::LBrace)?;
                let mut body = Vec::new();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    body.push(self.parse_stmt()?);
                }
                self.expect(&Tok::RBrace)?;
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
            Tok::LBrace => {
                self.next();
                let mut body = Vec::new();
                while !matches!(self.peek(), Tok::RBrace | Tok::Eof) {
                    body.push(self.parse_stmt()?);
                }
                self.expect(&Tok::RBrace)?;
                Ok(Stmt::Block(body))
            }
            _ => {
                let e = self.parse_expr()?;
                let _ = self.eat(&Tok::Semi);
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> Result<Expr, String> {
        let lhs = self.parse_or()?;
        if self.eat(&Tok::Eq) {
            let rhs = self.parse_assign()?;
            if let Expr::Var(name) = lhs {
                return Ok(Expr::Assign(name, Box::new(rhs)));
            }
            return Err("asignación inválida".into());
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
            Tok::Bang => {
                self.next();
                Ok(Expr::Unary(UnaryOp::Not, Box::new(self.parse_unary()?)))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.eat(&Tok::LParen) {
                let mut args = Vec::new();
                while !matches!(self.peek(), Tok::RParen | Tok::Eof) {
                    args.push(self.parse_expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(&Tok::RParen)?;
                if let Expr::Var(name) = expr {
                    expr = Expr::Call(name, args);
                } else {
                    return Err("llamada no soportada".into());
                }
            } else if self.eat(&Tok::LBracket) {
                let idx = self.parse_expr()?;
                self.expect(&Tok::RBracket)?;
                expr = Expr::Index(Box::new(expr), Box::new(idx));
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.next() {
            Tok::Num(n) => Ok(Expr::Lit(JsVal::Number(n))),
            Tok::Str(s) => Ok(Expr::Lit(JsVal::Str(s))),
            Tok::KwTrue => Ok(Expr::Lit(JsVal::Bool(true))),
            Tok::KwFalse => Ok(Expr::Lit(JsVal::Bool(false))),
            Tok::KwNull => Ok(Expr::Lit(JsVal::Null)),
            Tok::KwUndefined => Ok(Expr::Lit(JsVal::Undefined)),
            Tok::Ident(name) => Ok(Expr::Var(name)),
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
    Load(String),
    Store(String),
    MakeArray(usize),
    IndexGet,
    Unary(UnaryOp),
    Binary(BinOp),
    Jump(usize),
    JumpIfFalse(usize),
    Call(String, usize),
    Return,
}

#[derive(Debug, Clone)]
struct FunctionChunk {
    params: Vec<String>,
    code: Vec<Op>,
}

struct Compiler {
    functions: Vec<FunctionChunk>,
    fn_names: HashMap<String, usize>,
}

impl Compiler {
    fn new() -> Self {
        Self {
            functions: Vec::new(),
            fn_names: HashMap::new(),
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
                self.compile_expr(cond, code);
                let j_exit = code.len();
                code.push(Op::JumpIfFalse(0));
                self.compile_stmt(body, code, false);
                code.push(Op::Jump(loop_start));
                code[j_exit] = Op::JumpIfFalse(code.len());
                if keep_value {
                    code.push(Op::Push(JsVal::Undefined));
                }
            }
            Stmt::FnDecl(name, params, body) => {
                let mut fn_code = Vec::new();
                for s in body {
                    self.compile_stmt(s, &mut fn_code, false);
                }
                fn_code.push(Op::Push(JsVal::Undefined));
                fn_code.push(Op::Return);
                let id = self.functions.len();
                self.functions.push(FunctionChunk {
                    params: params.clone(),
                    code: fn_code,
                });
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
            Expr::Index(target, index) => {
                self.compile_expr(target, code);
                self.compile_expr(index, code);
                code.push(Op::IndexGet);
            }
            Expr::Assign(name, rhs) => {
                self.compile_expr(rhs, code);
                code.push(Op::Store(name.clone()));
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
            Expr::Call(name, args) => {
                for arg in args {
                    self.compile_expr(arg, code);
                }
                code.push(Op::Call(name.clone(), args.len()));
            }
        }
    }
}

/// Evalúa un fragmento JavaScript con el motor propio `wisp-js::vm`.
pub fn eval_native(src: &str) -> Result<JsVal, String> {
    let toks = tokenize(src)?;
    let stmts = Parser::new(toks).parse_program()?;
    let mut compiler = Compiler::new();
    let main_code = compiler.compile_program(&stmts);

    let mut globals: HashMap<String, JsVal> = HashMap::new();
    let mut steps = 0usize;
    exec_chunk(
        &main_code,
        &compiler.functions,
        &compiler.fn_names,
        &mut globals,
        &mut HashMap::new(),
        &mut steps,
    )
}

fn exec_chunk(
    code: &[Op],
    functions: &[FunctionChunk],
    fn_names: &HashMap<String, usize>,
    globals: &mut HashMap<String, JsVal>,
    locals: &mut HashMap<String, JsVal>,
    steps: &mut usize,
) -> Result<JsVal, String> {
    let mut stack: Vec<JsVal> = Vec::new();
    let mut ip = 0usize;

    while ip < code.len() {
        *steps += 1;
        if *steps > MAX_VM_STEPS {
            return Err("presupuesto de instrucciones de wisp-js agotado".into());
        }
        match &code[ip] {
            Op::Push(v) => stack.push(v.clone()),
            Op::Pop => {
                let _ = stack.pop();
            }
            Op::Load(name) => {
                let val = locals
                    .get(name)
                    .or_else(|| globals.get(name))
                    .cloned()
                    .unwrap_or(JsVal::Undefined);
                stack.push(val);
            }
            Op::Store(name) => {
                let val = stack.last().cloned().unwrap_or(JsVal::Undefined);
                if locals.contains_key(name)
                    || (!locals.is_empty() && !globals.contains_key(name))
                {
                    locals.insert(name.clone(), val);
                } else {
                    globals.insert(name.clone(), val);
                }
            }
            Op::MakeArray(n) => {
                let split = stack.len().saturating_sub(*n);
                let items = stack.split_off(split);
                stack.push(JsVal::Array(items));
            }
            Op::IndexGet => {
                let idx = stack.pop().unwrap_or(JsVal::Undefined);
                let target = stack.pop().unwrap_or(JsVal::Undefined);
                let val = match (target, idx) {
                    (JsVal::Array(items), JsVal::Number(n)) if n >= 0.0 => {
                        items.get(n as usize).cloned().unwrap_or(JsVal::Undefined)
                    }
                    _ => JsVal::Undefined,
                };
                stack.push(val);
            }
            Op::Unary(op) => {
                let v = stack.pop().unwrap_or(JsVal::Undefined);
                stack.push(match op {
                    UnaryOp::Neg => JsVal::Number(-v.to_number()),
                    UnaryOp::Not => JsVal::Bool(!v.is_truthy()),
                });
            }
            Op::Binary(op) => {
                let rhs = stack.pop().unwrap_or(JsVal::Undefined);
                let lhs = stack.pop().unwrap_or(JsVal::Undefined);
                stack.push(eval_binary(*op, lhs, rhs));
            }
            Op::Jump(target) => {
                ip = *target;
                continue;
            }
            Op::JumpIfFalse(target) => {
                let cond = stack.pop().unwrap_or(JsVal::Undefined);
                if !cond.is_truthy() {
                    ip = *target;
                    continue;
                }
            }
            Op::Call(name, argc) => {
                let split = stack.len().saturating_sub(*argc);
                let args = stack.split_off(split);
                let Some(&fn_id) = fn_names.get(name) else {
                    return Err(format!("función no definida `{name}`"));
                };
                let chunk = &functions[fn_id];
                let mut fn_locals = HashMap::new();
                for (param, arg) in chunk.params.iter().zip(args.into_iter()) {
                    fn_locals.insert(param.clone(), arg);
                }
                let ret = exec_chunk(
                    &chunk.code,
                    functions,
                    fn_names,
                    globals,
                    &mut fn_locals,
                    steps,
                )?;
                stack.push(ret);
            }
            Op::Return => {
                return Ok(stack.pop().unwrap_or(JsVal::Undefined));
            }
        }
        ip += 1;
    }

    Ok(stack.pop().unwrap_or(JsVal::Undefined))
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
                let i = 1;
                while (i <= n) {
                    acc = acc + i;
                    i = i + 1;
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
    fn supports_arrays_strings_and_conditionals() {
        let res = eval_native(
            r#"
            const xs = [10, 20, 30];
            if (xs[1] === 20) {
                "ok-" + xs[2];
            } else {
                "fail";
            }
            "#,
        )
        .unwrap();
        assert_eq!(res, JsVal::Str("ok-30".into()));
    }
}
