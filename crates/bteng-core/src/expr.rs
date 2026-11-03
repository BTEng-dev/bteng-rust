//! The precondition expression language.
//!
//! Deliberately small, and hand-written rather than pulled in: comparisons,
//! `&&` `||` `!` (and the spellings `and` `or` `not`), `+ - * / %`, number,
//! string and `true`/`false` literals, and bare names that mean blackboard keys.
//! No calls, no attribute access, no indexing, no assignment — each of those is
//! rejected by name at parse time, so a tree written for a richer scripting
//! language fails loudly instead of meaning something else here.
//!
//! # Three stages, two of them at build time
//!
//! 1. [`Expr::parse`] turns text into an unresolved syntax tree. Names are still
//!    strings, so a front end can rewrite them ([`Expr::rename`]) — which is how
//!    the XML loader puts a subtree's `{count}` and its `count` into the same
//!    namespace.
//! 2. [`Expr::compile`] resolves every name to a [`SlotId`] against the
//!    blackboard the tree is built with, and **type-checks the whole
//!    expression**. An unknown key, a key of a type the language cannot read,
//!    `"a" < 3`, or an expression that is not a boolean are all build errors.
//! 3. [`Compiled::eval`] runs on the tick path. It walks a flat, preallocated
//!    node array by index, reads slots in place, and allocates nothing. Its
//!    depth and size are bounded at parse time ([`MAX_DEPTH`], [`MAX_NODES`]),
//!    so one evaluation is bounded work.
//!
//! # Numbers
//!
//! Every number is an `f64` at evaluation time, whatever integer or float type
//! the slot holds. Integers past 2^53 lose precision; nothing a precondition
//! compares against is that large, and one numeric type means no integer
//! division by zero or overflow to report from inside a tick (`x / 0` is
//! infinity, `x % 0` is NaN, and every comparison with NaN is false).

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::TypeId;
use core::fmt;

use crate::blackboard::Blackboard;
use crate::id::SlotId;

/// Deepest nesting an expression may have. Evaluation recurses, so this is
/// what bounds its stack.
pub const MAX_DEPTH: usize = 32;

/// Most syntax nodes one expression may have. Bounds the work of one
/// evaluation.
pub const MAX_NODES: usize = 256;

/// Why an expression was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprError {
    /// The text is not a valid expression. `at` is a byte offset.
    Syntax { at: usize, message: String },
    /// A construct the language deliberately does not have.
    Forbidden { at: usize, what: &'static str },
    /// Nested deeper than [`MAX_DEPTH`] or longer than [`MAX_NODES`].
    TooComplex,
    /// A name that is not a blackboard key.
    UnknownKey { key: String },
    /// A blackboard key whose type the language cannot read.
    ///
    /// Readable types: `bool`, every primitive integer, `f32`, `f64`, `String`.
    UnsupportedKeyType { key: String },
    /// Operands of the wrong type, e.g. `"a" < 3` or `count && ready`.
    Type { message: String },
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExprError::Syntax { at, message } => write!(f, "at byte {at}: {message}"),
            ExprError::Forbidden { at, what } => {
                write!(f, "at byte {at}: {what} is not allowed in a precondition")
            }
            ExprError::TooComplex => write!(
                f,
                "expression is too complex (over {MAX_DEPTH} levels or {MAX_NODES} terms)"
            ),
            ExprError::UnknownKey { key } => write!(
                f,
                "unknown blackboard key {key:?}: declare it, or bind it to a port, before building"
            ),
            ExprError::UnsupportedKeyType { key } => write!(
                f,
                "blackboard key {key:?} holds a type a precondition cannot read \
                 (bool, integers, f32, f64 and String can be read)"
            ),
            ExprError::Type { message } => f.write_str(message),
        }
    }
}

impl core::error::Error for ExprError {}

/// What a renamed name becomes. See [`Expr::rename`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameSub {
    /// Another blackboard key.
    Key(String),
    /// A literal, read as `true`/`false`, a number, or else a string.
    Literal(String),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl BinOp {
    const fn symbol(self) -> &'static str {
        match self {
            BinOp::Or => "||",
            BinOp::And => "&&",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Ast {
    Num(f64),
    Str(Box<str>),
    Bool(bool),
    Name(Box<str>),
    Not(u16),
    Neg(u16),
    Bin(BinOp, u16, u16),
}

/// A parsed, not yet resolved, expression.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    nodes: Vec<Ast>,
    root: u16,
    source: Box<str>,
}

impl Expr {
    /// Parse `source`.
    ///
    /// # Errors
    /// [`ExprError::Syntax`], [`ExprError::Forbidden`] or
    /// [`ExprError::TooComplex`].
    pub fn parse(source: &str) -> Result<Expr, ExprError> {
        let tokens = lex(source)?;
        let mut p = Parser {
            tokens,
            pos: 0,
            nodes: Vec::new(),
            depth: 0,
        };
        let root = p.or()?.0;
        if let Some(t) = p.tokens.get(p.pos) {
            return Err(match t.tok {
                Tok::LParen => ExprError::Forbidden {
                    at: t.at,
                    what: "a function call",
                },
                _ => ExprError::Syntax {
                    at: t.at,
                    message: "unexpected text after the end of the expression".to_string(),
                },
            });
        }
        // Evaluation recurses over the tree, and `a && b && c ...` is as deep
        // as it is long even with no parentheses — so the limit is on the
        // tree's height, not only on the parser's nesting.
        if height(&p.nodes) > MAX_DEPTH {
            return Err(ExprError::TooComplex);
        }
        Ok(Expr {
            nodes: p.nodes,
            root,
            source: source.into(),
        })
    }

    /// The text this was parsed from.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Every blackboard key the expression names, in order of appearance.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.nodes.iter().filter_map(|n| match n {
            Ast::Name(name) => Some(&**name),
            _ => None,
        })
    }

    /// Rewrite names. `f` returns `None` to leave a name as it is.
    ///
    /// Build time only. The XML loader uses it to apply subtree key remapping,
    /// so an expression and a port inside the same subtree see the same keys.
    pub fn rename(&mut self, mut f: impl FnMut(&str) -> Option<NameSub>) {
        for node in &mut self.nodes {
            if let Ast::Name(name) = node
                && let Some(sub) = f(name)
            {
                *node = match sub {
                    NameSub::Key(k) => Ast::Name(k.into()),
                    NameSub::Literal(lit) => literal_ast(&lit),
                };
            }
        }
    }

    /// Resolve every name against `bb` and type-check.
    ///
    /// # Errors
    /// [`ExprError::UnknownKey`], [`ExprError::UnsupportedKeyType`] or
    /// [`ExprError::Type`] — including when the expression is not a boolean.
    pub fn compile(&self, bb: &Blackboard) -> Result<Compiled, ExprError> {
        let mut ops = Vec::with_capacity(self.nodes.len());
        let mut types = Vec::with_capacity(self.nodes.len());
        // Children always precede their parent in `nodes` (the parser pushes a
        // node after its operands), so one forward pass sees every operand's
        // type before it is needed.
        for node in &self.nodes {
            let (op, ty) = compile_one(node, &types, bb)?;
            ops.push(op);
            types.push(ty);
        }
        let root_ty = types[usize::from(self.root)];
        if root_ty != Ty::Bool {
            return Err(ExprError::Type {
                message: alloc::format!(
                    "a precondition must be true or false, but this is a {}",
                    root_ty.name()
                ),
            });
        }
        Ok(Compiled {
            ops: ops.into_boxed_slice(),
            root: self.root,
        })
    }
}

/// Height of the tree. Operands precede their parent, so one forward pass works.
fn height(nodes: &[Ast]) -> usize {
    let mut h: Vec<usize> = Vec::with_capacity(nodes.len());
    for n in nodes {
        let of = |i: &u16| h[usize::from(*i)];
        let v = match n {
            Ast::Not(a) | Ast::Neg(a) => of(a) + 1,
            Ast::Bin(_, a, b) => of(a).max(of(b)) + 1,
            _ => 1,
        };
        h.push(v);
    }
    h.iter().copied().max().unwrap_or(0)
}

fn literal_ast(text: &str) -> Ast {
    let t = text.trim();
    match t {
        "true" => Ast::Bool(true),
        "false" => Ast::Bool(false),
        _ => match t.parse::<f64>() {
            Ok(n) => Ast::Num(n),
            Err(_) => Ast::Str(text.into()),
        },
    }
}

// ── lexer ────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Op(BinOp),
    Not,
    Minus,
    LParen,
    RParen,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    at: usize,
}

fn lex(src: &str) -> Result<Vec<Token>, ExprError> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let at = i;
        let two = |s: &[u8]| bytes[i..].starts_with(s);
        let tok = match c {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            b'(' => {
                i += 1;
                Tok::LParen
            }
            b')' => {
                i += 1;
                Tok::RParen
            }
            b'&' if two(b"&&") => {
                i += 2;
                Tok::Op(BinOp::And)
            }
            b'|' if two(b"||") => {
                i += 2;
                Tok::Op(BinOp::Or)
            }
            b'=' if two(b"==") => {
                i += 2;
                Tok::Op(BinOp::Eq)
            }
            b'!' if two(b"!=") => {
                i += 2;
                Tok::Op(BinOp::Ne)
            }
            b'<' if two(b"<=") => {
                i += 2;
                Tok::Op(BinOp::Le)
            }
            b'>' if two(b">=") => {
                i += 2;
                Tok::Op(BinOp::Ge)
            }
            b'!' => {
                i += 1;
                Tok::Not
            }
            b'<' => {
                i += 1;
                Tok::Op(BinOp::Lt)
            }
            b'>' => {
                i += 1;
                Tok::Op(BinOp::Gt)
            }
            b'+' => {
                i += 1;
                Tok::Op(BinOp::Add)
            }
            b'-' => {
                i += 1;
                Tok::Minus
            }
            b'*' => {
                i += 1;
                Tok::Op(BinOp::Mul)
            }
            b'/' => {
                i += 1;
                Tok::Op(BinOp::Div)
            }
            b'%' => {
                i += 1;
                Tok::Op(BinOp::Rem)
            }
            b'=' => {
                return Err(ExprError::Forbidden {
                    at,
                    what: "assignment (`=`; did you mean `==`?)",
                });
            }
            b':' if two(b":=") => {
                return Err(ExprError::Forbidden {
                    at,
                    what: "assignment (`:=`)",
                });
            }
            b'.' if !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                return Err(ExprError::Forbidden {
                    at,
                    what: "attribute access (`.`)",
                });
            }
            b'[' | b']' => {
                return Err(ExprError::Forbidden {
                    at,
                    what: "indexing (`[]`)",
                });
            }
            b'\'' | b'"' => {
                let (s, next) = lex_string(src, i)?;
                i = next;
                Tok::Str(s)
            }
            b'0'..=b'9' | b'.' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
                    let mut j = i + 1;
                    if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j].is_ascii_digit() {
                        i = j;
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                let text = &src[start..i];
                let n = text.parse::<f64>().map_err(|_| ExprError::Syntax {
                    at,
                    message: alloc::format!("{text:?} is not a number"),
                })?;
                if i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                    return Err(ExprError::Syntax {
                        at,
                        message: "a name cannot start with a digit".to_string(),
                    });
                }
                Tok::Num(n)
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                // Caught here, before the argument list can trip the lexer
                // on a `,` and produce a less useful message.
                let rest = src[i..].trim_start();
                if rest.starts_with('(') && !matches!(&src[start..i], "and" | "or" | "not") {
                    return Err(ExprError::Forbidden {
                        at,
                        what: "a function call",
                    });
                }
                match &src[start..i] {
                    "and" => Tok::Op(BinOp::And),
                    "or" => Tok::Op(BinOp::Or),
                    "not" => Tok::Not,
                    word => Tok::Ident(word.to_string()),
                }
            }
            _ => {
                let ch = src[i..].chars().next().unwrap_or('?');
                return Err(ExprError::Syntax {
                    at,
                    message: alloc::format!("unexpected character {ch:?}"),
                });
            }
        };
        out.push(Token { tok, at });
        if out.len() > MAX_NODES {
            return Err(ExprError::TooComplex);
        }
    }
    Ok(out)
}

/// A quoted string starting at `start`. Backslash escapes the quote and itself.
fn lex_string(src: &str, start: usize) -> Result<(String, usize), ExprError> {
    let bytes = src.as_bytes();
    let quote = bytes[start];
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        let c = bytes[i];
        if c == quote {
            return Ok((out, i + 1));
        }
        if c == b'\\' && i + 1 < bytes.len() && matches!(bytes[i + 1], b'\\' | b'\'' | b'"') {
            out.push(char::from(bytes[i + 1]));
            i += 2;
            continue;
        }
        let ch = src[i..].chars().next().unwrap_or('?');
        out.push(ch);
        i += ch.len_utf8();
    }
    Err(ExprError::Syntax {
        at: start,
        message: "unterminated string".to_string(),
    })
}

// ── parser ───────────────────────────────────────────────────────────────────
//
//   or    := and  ( ("||" | "or")  and )*
//   and   := cmp  ( ("&&" | "and") cmp )*
//   cmp   := add  ( cmpop add )?            -- not chainable
//   add   := mul  ( ("+" | "-") mul )*
//   mul   := unary ( ("*" | "/" | "%") unary )*
//   unary := ("!" | "not" | "-") unary | primary
//   primary := number | string | true | false | name | "(" or ")"
//
// `!` and `not` bind like C's `!`. Python's `not` binds looser than `==`, so
// `not a == b` means different things in the two languages; rather than pick
// one silently, an unparenthesised negation on the left of a comparison is an
// error asking for parentheses. `a < b < c` is rejected for the same reason
// (Python chains it, C does not).

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    nodes: Vec<Ast>,
    depth: usize,
}

/// A parsed operand, and whether it is a bare `!x` / `not x`.
type Parsed = (u16, bool);

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|t| &t.tok)
    }

    fn at(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map_or_else(|| self.tokens.last().map_or(0, |t| t.at + 1), |t| t.at)
    }

    fn push(&mut self, node: Ast) -> Result<u16, ExprError> {
        if self.nodes.len() >= MAX_NODES {
            return Err(ExprError::TooComplex);
        }
        self.nodes.push(node);
        Ok((self.nodes.len() - 1) as u16)
    }

    fn enter(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ExprError::TooComplex);
        }
        Ok(())
    }

    fn binary_level(
        &mut self,
        ops: &[BinOp],
        next: fn(&mut Parser) -> Result<Parsed, ExprError>,
    ) -> Result<Parsed, ExprError> {
        let mut left = next(self)?;
        while let Some(Tok::Op(op)) = self.peek() {
            let op = *op;
            if !ops.contains(&op) {
                break;
            }
            self.pos += 1;
            let right = next(self)?.0;
            left = (self.push(Ast::Bin(op, left.0, right))?, false);
        }
        Ok(left)
    }

    fn or(&mut self) -> Result<Parsed, ExprError> {
        self.binary_level(&[BinOp::Or], Parser::and)
    }

    fn and(&mut self) -> Result<Parsed, ExprError> {
        self.binary_level(&[BinOp::And], Parser::cmp)
    }

    fn cmp(&mut self) -> Result<Parsed, ExprError> {
        const CMP: &[BinOp] = &[
            BinOp::Eq,
            BinOp::Ne,
            BinOp::Lt,
            BinOp::Le,
            BinOp::Gt,
            BinOp::Ge,
        ];
        let left = self.add()?;
        let Some(Tok::Op(op)) = self.peek() else {
            return Ok(left);
        };
        let op = *op;
        if !CMP.contains(&op) {
            return Ok(left);
        }
        if left.1 {
            return Err(ExprError::Syntax {
                at: self.at(),
                message: alloc::format!(
                    "ambiguous: write `!(a {0} b)` or `(!a) {0} b`",
                    op.symbol()
                ),
            });
        }
        self.pos += 1;
        let right = self.add()?.0;
        if let Some(Tok::Op(next)) = self.peek()
            && CMP.contains(next)
        {
            return Err(ExprError::Syntax {
                at: self.at(),
                message: "comparisons do not chain; join them with `&&`".to_string(),
            });
        }
        Ok((self.push(Ast::Bin(op, left.0, right))?, false))
    }

    /// `-` is lexed as `Minus` (it may be unary), so this level accepts it as
    /// binary subtraction alongside `+`.
    fn add(&mut self) -> Result<Parsed, ExprError> {
        let mut left = self.mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Op(BinOp::Add)) => BinOp::Add,
                Some(Tok::Minus) => BinOp::Sub,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.mul()?.0;
            left = (self.push(Ast::Bin(op, left.0, right))?, false);
        }
    }

    fn mul(&mut self) -> Result<Parsed, ExprError> {
        self.binary_level(&[BinOp::Mul, BinOp::Div, BinOp::Rem], Parser::unary)
    }

    fn unary(&mut self) -> Result<Parsed, ExprError> {
        self.enter()?;
        let out = match self.peek() {
            Some(Tok::Not) => {
                self.pos += 1;
                let inner = self.unary()?.0;
                (self.push(Ast::Not(inner))?, true)
            }
            Some(Tok::Minus) => {
                self.pos += 1;
                let inner = self.unary()?.0;
                (self.push(Ast::Neg(inner))?, false)
            }
            _ => (self.primary()?, false),
        };
        self.depth -= 1;
        Ok(out)
    }

    fn primary(&mut self) -> Result<u16, ExprError> {
        let at = self.at();
        let Some(tok) = self.peek().cloned() else {
            return Err(ExprError::Syntax {
                at,
                message: "expression ends too early".to_string(),
            });
        };
        self.pos += 1;
        match tok {
            Tok::Num(n) => self.push(Ast::Num(n)),
            Tok::Str(s) => self.push(Ast::Str(s.into())),
            Tok::Ident(name) => {
                if let Some(Tok::LParen) = self.peek() {
                    return Err(ExprError::Forbidden {
                        at: self.at(),
                        what: "a function call",
                    });
                }
                match name.as_str() {
                    "true" => self.push(Ast::Bool(true)),
                    "false" => self.push(Ast::Bool(false)),
                    _ => self.push(Ast::Name(name.into())),
                }
            }
            Tok::LParen => {
                let inner = self.or()?.0;
                if self.peek() != Some(&Tok::RParen) {
                    return Err(ExprError::Syntax {
                        at: self.at(),
                        message: "missing `)`".to_string(),
                    });
                }
                self.pos += 1;
                Ok(inner)
            }
            Tok::RParen | Tok::Op(_) | Tok::Not | Tok::Minus => Err(ExprError::Syntax {
                at,
                message: "expected a value".to_string(),
            }),
        }
    }
}

// ── compiled form ────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Ty {
    Bool,
    Num,
    Str,
}

impl Ty {
    const fn name(self) -> &'static str {
        match self {
            Ty::Bool => "boolean",
            Ty::Num => "number",
            Ty::Str => "string",
        }
    }
}

/// How to read a slot. Fixed at build time: a slot's type never changes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SlotKind {
    Bool,
    I8,
    I16,
    I32,
    I64,
    Isize,
    U8,
    U16,
    U32,
    U64,
    Usize,
    F32,
    F64,
    Str,
}

impl SlotKind {
    fn of(t: TypeId) -> Option<SlotKind> {
        macro_rules! pick {
            ($($ty:ty => $k:ident),* $(,)?) => {
                $(if t == TypeId::of::<$ty>() { return Some(SlotKind::$k); })*
            };
        }
        pick!(
            bool => Bool, i8 => I8, i16 => I16, i32 => I32, i64 => I64, isize => Isize,
            u8 => U8, u16 => U16, u32 => U32, u64 => U64, usize => Usize,
            f32 => F32, f64 => F64, String => Str,
        );
        None
    }

    const fn ty(self) -> Ty {
        match self {
            SlotKind::Bool => Ty::Bool,
            SlotKind::Str => Ty::Str,
            _ => Ty::Num,
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Bool(bool),
    Num(f64),
    Str(Box<str>),
    Slot(SlotId, SlotKind),
    Not(u16),
    Neg(u16),
    And(u16, u16),
    Or(u16, u16),
    /// Comparison of two operands of the type given.
    Cmp(BinOp, Ty, u16, u16),
    Arith(BinOp, u16, u16),
}

fn compile_one(node: &Ast, types: &[Ty], bb: &Blackboard) -> Result<(Op, Ty), ExprError> {
    let ty = |i: &u16| types[usize::from(*i)];
    let mismatch = |message: String| Err(ExprError::Type { message });
    Ok(match node {
        Ast::Num(n) => (Op::Num(*n), Ty::Num),
        Ast::Str(s) => (Op::Str(s.clone()), Ty::Str),
        Ast::Bool(b) => (Op::Bool(*b), Ty::Bool),
        Ast::Name(name) => {
            let slot = bb.resolve(name).ok_or_else(|| ExprError::UnknownKey {
                key: name.to_string(),
            })?;
            let kind = bb.slot_type(slot).and_then(SlotKind::of).ok_or_else(|| {
                ExprError::UnsupportedKeyType {
                    key: name.to_string(),
                }
            })?;
            (Op::Slot(slot, kind), kind.ty())
        }
        Ast::Not(i) => {
            if ty(i) != Ty::Bool {
                return mismatch(alloc::format!(
                    "`!` needs a boolean, not a {}",
                    ty(i).name()
                ));
            }
            (Op::Not(*i), Ty::Bool)
        }
        Ast::Neg(i) => {
            if ty(i) != Ty::Num {
                return mismatch(alloc::format!(
                    "unary `-` needs a number, not a {}",
                    ty(i).name()
                ));
            }
            (Op::Neg(*i), Ty::Num)
        }
        Ast::Bin(op, l, r) => {
            let (lt, rt) = (ty(l), ty(r));
            match op {
                BinOp::And | BinOp::Or => {
                    if lt != Ty::Bool || rt != Ty::Bool {
                        return mismatch(alloc::format!(
                            "`{}` needs two booleans, got a {} and a {}",
                            op.symbol(),
                            lt.name(),
                            rt.name()
                        ));
                    }
                    let op = if *op == BinOp::And {
                        Op::And(*l, *r)
                    } else {
                        Op::Or(*l, *r)
                    };
                    (op, Ty::Bool)
                }
                BinOp::Eq | BinOp::Ne => {
                    if lt != rt {
                        return mismatch(alloc::format!(
                            "`{}` compares a {} with a {}",
                            op.symbol(),
                            lt.name(),
                            rt.name()
                        ));
                    }
                    (Op::Cmp(*op, lt, *l, *r), Ty::Bool)
                }
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    if lt != rt || lt == Ty::Bool {
                        return mismatch(alloc::format!(
                            "`{}` needs two numbers or two strings, got a {} and a {}",
                            op.symbol(),
                            lt.name(),
                            rt.name()
                        ));
                    }
                    (Op::Cmp(*op, lt, *l, *r), Ty::Bool)
                }
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                    // No string `+`: concatenation would allocate on the tick.
                    if lt != Ty::Num || rt != Ty::Num {
                        return mismatch(alloc::format!(
                            "`{}` needs two numbers, got a {} and a {}",
                            op.symbol(),
                            lt.name(),
                            rt.name()
                        ));
                    }
                    (Op::Arith(*op, *l, *r), Ty::Num)
                }
            }
        }
    })
}

/// A resolved, type-checked expression, ready for the tick path.
///
/// A flat array of operations addressed by `u16` index — built once, never
/// resized, and walked without allocating.
#[derive(Clone, Debug)]
pub struct Compiled {
    ops: Box<[Op]>,
    root: u16,
}

impl Compiled {
    /// Evaluate against `bb`.
    ///
    /// `None` only if a slot cannot be read as the type it had at build time —
    /// which means the tree is being ticked against a different blackboard than
    /// the one it was built with. Allocation-free and bounded by [`MAX_NODES`].
    #[must_use]
    pub fn eval(&self, bb: &Blackboard) -> Option<bool> {
        self.bool_at(self.root, bb)
    }

    fn bool_at(&self, i: u16, bb: &Blackboard) -> Option<bool> {
        Some(match &self.ops[usize::from(i)] {
            Op::Bool(b) => *b,
            Op::Slot(slot, SlotKind::Bool) => *bb.get::<bool>(*slot).ok()?,
            Op::Not(a) => !self.bool_at(*a, bb)?,
            Op::And(a, b) => self.bool_at(*a, bb)? && self.bool_at(*b, bb)?,
            Op::Or(a, b) => self.bool_at(*a, bb)? || self.bool_at(*b, bb)?,
            Op::Cmp(op, Ty::Num, a, b) => {
                let (x, y) = (self.num_at(*a, bb)?, self.num_at(*b, bb)?);
                match op {
                    BinOp::Eq => x == y,
                    BinOp::Ne => x != y,
                    BinOp::Lt => x < y,
                    BinOp::Le => x <= y,
                    BinOp::Gt => x > y,
                    _ => x >= y,
                }
            }
            Op::Cmp(op, Ty::Str, a, b) => {
                let (x, y) = (self.str_at(*a, bb)?, self.str_at(*b, bb)?);
                match op {
                    BinOp::Eq => x == y,
                    BinOp::Ne => x != y,
                    BinOp::Lt => x < y,
                    BinOp::Le => x <= y,
                    BinOp::Gt => x > y,
                    _ => x >= y,
                }
            }
            Op::Cmp(op, Ty::Bool, a, b) => {
                let (x, y) = (self.bool_at(*a, bb)?, self.bool_at(*b, bb)?);
                if *op == BinOp::Eq { x == y } else { x != y }
            }
            // The type checker only lets booleans reach here.
            _ => return None,
        })
    }

    fn num_at(&self, i: u16, bb: &Blackboard) -> Option<f64> {
        macro_rules! read {
            ($slot:expr, $t:ty) => {
                *bb.get::<$t>(*$slot).ok()? as f64
            };
        }
        Some(match &self.ops[usize::from(i)] {
            Op::Num(n) => *n,
            Op::Slot(s, kind) => match kind {
                SlotKind::I8 => read!(s, i8),
                SlotKind::I16 => read!(s, i16),
                SlotKind::I32 => read!(s, i32),
                SlotKind::I64 => read!(s, i64),
                SlotKind::Isize => read!(s, isize),
                SlotKind::U8 => read!(s, u8),
                SlotKind::U16 => read!(s, u16),
                SlotKind::U32 => read!(s, u32),
                SlotKind::U64 => read!(s, u64),
                SlotKind::Usize => read!(s, usize),
                SlotKind::F32 => read!(s, f32),
                SlotKind::F64 => read!(s, f64),
                SlotKind::Bool | SlotKind::Str => return None,
            },
            Op::Neg(a) => -self.num_at(*a, bb)?,
            Op::Arith(op, a, b) => {
                let (x, y) = (self.num_at(*a, bb)?, self.num_at(*b, bb)?);
                match op {
                    BinOp::Add => x + y,
                    BinOp::Sub => x - y,
                    BinOp::Mul => x * y,
                    BinOp::Div => x / y,
                    _ => x % y,
                }
            }
            _ => return None,
        })
    }

    fn str_at<'b>(&'b self, i: u16, bb: &'b Blackboard) -> Option<&'b str> {
        match &self.ops[usize::from(i)] {
            Op::Str(s) => Some(s),
            Op::Slot(slot, SlotKind::Str) => bb.get::<String>(*slot).ok().map(String::as_str),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bb() -> Blackboard {
        let mut bb = Blackboard::new();
        bb.declare("battery", 15i32).unwrap();
        bb.declare("level", 0.5f64).unwrap();
        bb.declare("ready", true).unwrap();
        bb.declare("state", String::from("DOCKED")).unwrap();
        bb.declare("pose", (1.0f64, 2.0f64)).unwrap();
        bb
    }

    fn eval(src: &str) -> bool {
        let bb = bb();
        Expr::parse(src)
            .unwrap_or_else(|e| panic!("{src}: {e}"))
            .compile(&bb)
            .unwrap_or_else(|e| panic!("{src}: {e}"))
            .eval(&bb)
            .unwrap()
    }

    fn err(src: &str) -> ExprError {
        let bb = bb();
        match Expr::parse(src) {
            Err(e) => e,
            Ok(e) => e.compile(&bb).expect_err(src),
        }
    }

    #[test]
    fn evaluates() {
        assert!(eval("battery < 20"));
        assert!(!eval("battery >= 20"));
        assert!(eval("state == 'DOCKED'"));
        assert!(eval("state != \"UNDOCKED\""));
        assert!(eval("ready && battery == 15"));
        assert!(eval("!ready || level * 2 == 1"));
        assert!(eval("battery % 4 == 3 and -battery < 0"));
        assert!(eval("(battery + 5) / 4 == 5"));
        assert!(eval("battery - 5 - 5 == 5"));
        assert!(eval("not (battery > 20)"));
        assert!(eval("true != false"));
        assert!(eval("1e1 == 10 && .5 == level"));
        assert!(eval("'a' < 'b'"));
    }

    #[test]
    fn rejects() {
        assert!(matches!(
            err("max(battery, 3) > 1"),
            ExprError::Forbidden { .. }
        ));
        assert!(matches!(err("pose.x > 1"), ExprError::Forbidden { .. }));
        assert!(matches!(err("pose[0] > 1"), ExprError::Forbidden { .. }));
        assert!(matches!(err("battery = 3"), ExprError::Forbidden { .. }));
        assert!(matches!(err("battery := 3"), ExprError::Forbidden { .. }));
        assert!(matches!(err("not ready == true"), ExprError::Syntax { .. }));
        assert!(matches!(err("1 < battery < 3"), ExprError::Syntax { .. }));
        assert!(matches!(err("battery <"), ExprError::Syntax { .. }));
        assert!(matches!(err("(ready"), ExprError::Syntax { .. }));
        assert!(matches!(err("'open"), ExprError::Syntax { .. }));
        assert_eq!(
            err("missing > 1"),
            ExprError::UnknownKey {
                key: "missing".into()
            }
        );
        assert!(matches!(
            err("pose == 1"),
            ExprError::UnsupportedKeyType { .. }
        ));
        assert!(matches!(err("battery"), ExprError::Type { .. }));
        assert!(matches!(err("state < 3"), ExprError::Type { .. }));
        assert!(matches!(err("state + 'x' == 'y'"), ExprError::Type { .. }));
        assert!(matches!(err("ready < true"), ExprError::Type { .. }));
        let deep = alloc::format!("{}ready{}", "(".repeat(40), ")".repeat(40));
        assert_eq!(err(&deep), ExprError::TooComplex);
    }

    #[test]
    fn renames() {
        let bb = bb();
        let mut e = Expr::parse("b < limit").unwrap();
        e.rename(|n| match n {
            "b" => Some(NameSub::Key("battery".into())),
            "limit" => Some(NameSub::Literal("20".into())),
            _ => None,
        });
        assert_eq!(e.compile(&bb).unwrap().eval(&bb), Some(true));
    }
}
