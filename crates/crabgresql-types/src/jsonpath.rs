//! `jsonpath` type: the SQL/JSON path language (`$.a[*] ? (@ > 3)`).
//!
//! Clean-room reproduction of PostgreSQL's observable behavior (I/O text, error
//! text, SQLSTATE) for `jsonpath` and the `jsonb_path_*` functions. Hand-written
//! lexer + recursive-descent parser and a tree-walking evaluator over the
//! [`crate::json::Jsonb`] canonical tree — no external dependency beyond the
//! `regex` crate (already used by [`crate::text`]) for `like_regex`.
//!
//! Two halves:
//!
//! * **Parsing** ([`jsonpath_in`]) turns the path text into a [`JsonPath`] program
//!   ({ `strict`, root [`Node`] }) and [`format`] renders PG's canonical spelling
//!   (`jsonpath_out`) — keys always double-quoted, arithmetic fully parenthesized.
//! * **Evaluation** ([`query`]/[`exists`]/[`match_predicate`]) walks the program
//!   against a `jsonb` document producing an SQL/JSON sequence. `lax` mode (the
//!   default) auto-unwraps arrays and suppresses structural errors; `strict` mode
//!   raises them (PG's `2203A`/`22039`/… SQLSTATEs). A `silent` flag suppresses
//!   the same errors at the top level — but never a missing-variable error
//!   (`42704`), matching PG.

use crate::json::{JsonError, Jsonb};
use crate::numeric::Numeric;
use std::cmp::Ordering;

// SQLSTATE literals (kept local; the types crate must not depend on the wire
// crate). Mirror `crabgresql_pg_wire::sqlstate`.
const SYNTAX_ERROR: &str = "42601";
const FEATURE_NOT_SUPPORTED: &str = "0A000";
const UNDEFINED_OBJECT: &str = "42704";
const DIVISION_BY_ZERO: &str = "22012";
const SQL_JSON_MEMBER_NOT_FOUND: &str = "2203A";
const SQL_JSON_NUMBER_NOT_FOUND: &str = "2203B";
const SQL_JSON_ARRAY_NOT_FOUND: &str = "22039";
const SQL_JSON_ITEM_METHOD: &str = "22036";
const SINGLETON_JSON_ITEM_REQUIRED: &str = "22038";

/// Guard against a pathologically deep path chain overflowing the stack while
/// parsing or evaluating (worker threads get ~2 MB stacks). Far exceeds any real
/// path; PG relies on `check_stack_depth()` for the same protection.
const MAX_DEPTH: usize = 200;

/// A parsed `jsonpath` program: a mode flag plus the root expression.
#[derive(deepsize::DeepSizeOf, Clone, Debug, PartialEq, Hash)]
pub struct JsonPath {
    strict: bool,
    expr: Node,
}

/// A node of the jsonpath expression tree. Value/path nodes evaluate to an
/// SQL/JSON sequence; the boolean-predicate nodes (Compare/And/Or/Not/Exists/…)
/// evaluate to a three-valued [`Ternary`].
#[derive(deepsize::DeepSizeOf, Clone, Debug, PartialEq, Hash)]
enum Node {
    Root,
    Current,
    Last,
    Var(String),
    LitNum(Numeric),
    LitStr(String),
    LitBool(bool),
    LitNull,
    /// A base expression followed by one accessor step (`base.key`, `base[*]`, …).
    Accessor {
        base: Box<Node>,
        step: Accessor,
    },
    Unary {
        neg: bool,
        operand: Box<Node>,
    },
    Arith {
        op: ArithOp,
        left: Box<Node>,
        right: Box<Node>,
    },
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
    Compare {
        op: CmpOp,
        left: Box<Node>,
        right: Box<Node>,
    },
    Exists(Box<Node>),
    StartsWith {
        operand: Box<Node>,
        prefix: Box<Node>,
    },
    LikeRegex {
        operand: Box<Node>,
        pattern: String,
        flags: crate::text::LikeRegexFlags,
    },
    IsUnknown(Box<Node>),
}

#[derive(deepsize::DeepSizeOf, Clone, Debug, PartialEq, Hash)]
enum Accessor {
    /// `.key` or `."key"`.
    Key(String),
    /// `.*`.
    WildcardMember,
    /// `[*]`.
    WildcardArray,
    /// `.**` with an optional `{lo}` / `{lo to hi}` level range.
    Recursive(Option<u32>, Option<u32>),
    /// `[a, b to c, ...]`.
    Subscript(Vec<Subscript>),
    Method(Method),
    /// `? (predicate)`.
    Filter(Box<Node>),
}

#[derive(deepsize::DeepSizeOf, Clone, Debug, PartialEq, Hash)]
enum Subscript {
    Index(Node),
    Range(Node, Node),
}

#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Hash)]
enum Method {
    Size,
    Type,
    Double,
    Abs,
    Floor,
    Ceiling,
    KeyValue,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Size => "size",
            Method::Type => "type",
            Method::Double => "double",
            Method::Abs => "abs",
            Method::Floor => "floor",
            Method::Ceiling => "ceiling",
            Method::KeyValue => "keyvalue",
        }
    }
}

#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Hash)]
enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl ArithOp {
    fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Mod => "%",
        }
    }
}

#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Hash)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn err(sqlstate: &'static str, message: impl Into<String>) -> JsonError {
    JsonError {
        sqlstate,
        message: message.into(),
        detail: None,
    }
}

fn syntax(message: impl Into<String>) -> JsonError {
    err(SYNTAX_ERROR, message)
}

/// Carry a `crate::text` failure through unchanged: its SQLSTATE and message
/// are already the ones PG reports for a bad regex.
fn text_err(e: crate::text::TextError) -> JsonError {
    JsonError {
        sqlstate: e.sqlstate,
        message: e.message,
        detail: None,
    }
}

/// Whether `silent`/lax suppression applies to this error. PG suppresses every
/// structural jsonpath error under `silent` and inside filter predicates, but a
/// missing-variable error (`42704`) is always raised.
fn suppressible(e: &JsonError) -> bool {
    e.sqlstate != UNDEFINED_OBJECT
}

// ===========================================================================
// Lexer
// ===========================================================================

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Dollar,
    At,
    Dot,
    Star,
    StarStar,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Question,
    Bang,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Num(Numeric),
    Str(String),
    /// A bareword identifier (a key after `.`, a keyword, or a method name).
    Ident(String),
    /// `$name` or `$"quoted"` — a named variable reference.
    Var(String),
}

struct Lexer<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a str) -> Self {
        Lexer {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.b.get(self.i + 1).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn tokenize(mut self) -> Result<Vec<Tok>, JsonError> {
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            let Some(c) = self.peek() else { break };
            let tok = match c {
                b'$' => {
                    self.i += 1;
                    // `$name` / `$"quoted"` is a variable; a bare `$` is the root.
                    match self.peek() {
                        Some(b'"') => Tok::Var(self.lex_quoted()?),
                        Some(c) if is_ident_start(c) => Tok::Var(self.lex_bareword()),
                        _ => Tok::Dollar,
                    }
                }
                b'@' => {
                    self.i += 1;
                    Tok::At
                }
                b'.' => {
                    self.i += 1;
                    Tok::Dot
                }
                b'*' => {
                    self.i += 1;
                    if self.peek() == Some(b'*') {
                        self.i += 1;
                        Tok::StarStar
                    } else {
                        Tok::Star
                    }
                }
                b'(' => {
                    self.i += 1;
                    Tok::LParen
                }
                b')' => {
                    self.i += 1;
                    Tok::RParen
                }
                b'[' => {
                    self.i += 1;
                    Tok::LBracket
                }
                b']' => {
                    self.i += 1;
                    Tok::RBracket
                }
                b'{' => {
                    self.i += 1;
                    Tok::LBrace
                }
                b'}' => {
                    self.i += 1;
                    Tok::RBrace
                }
                b',' => {
                    self.i += 1;
                    Tok::Comma
                }
                b'?' => {
                    self.i += 1;
                    Tok::Question
                }
                b'+' => {
                    self.i += 1;
                    Tok::Plus
                }
                b'-' => {
                    self.i += 1;
                    Tok::Minus
                }
                b'/' => {
                    self.i += 1;
                    Tok::Slash
                }
                b'%' => {
                    self.i += 1;
                    Tok::Percent
                }
                b'=' => {
                    if self.peek2() == Some(b'=') {
                        self.i += 2;
                        Tok::Eq
                    } else {
                        return Err(self.near_error());
                    }
                }
                b'!' => {
                    if self.peek2() == Some(b'=') {
                        self.i += 2;
                        Tok::Ne
                    } else {
                        self.i += 1;
                        Tok::Bang
                    }
                }
                b'<' => {
                    if self.peek2() == Some(b'=') {
                        self.i += 2;
                        Tok::Le
                    } else if self.peek2() == Some(b'>') {
                        self.i += 2;
                        Tok::Ne
                    } else {
                        self.i += 1;
                        Tok::Lt
                    }
                }
                b'>' => {
                    if self.peek2() == Some(b'=') {
                        self.i += 2;
                        Tok::Ge
                    } else {
                        self.i += 1;
                        Tok::Gt
                    }
                }
                b'&' => {
                    if self.peek2() == Some(b'&') {
                        self.i += 2;
                        Tok::AndAnd
                    } else {
                        return Err(self.near_error());
                    }
                }
                b'|' => {
                    if self.peek2() == Some(b'|') {
                        self.i += 2;
                        Tok::OrOr
                    } else {
                        return Err(self.near_error());
                    }
                }
                b'"' => Tok::Str(self.lex_quoted()?),
                b'0'..=b'9' => Tok::Num(self.lex_number()?),
                c if is_ident_start(c) => Tok::Ident(self.lex_bareword()),
                _ => return Err(self.near_error()),
            };
            out.push(tok);
        }
        Ok(out)
    }

    /// A `syntax error at or near "<token>" of jsonpath input`, using the run of
    /// non-whitespace at the cursor as the offending token (PG's wording).
    fn near_error(&self) -> JsonError {
        let start = self.i;
        let mut end = start;
        while let Some(c) = self.b.get(end).copied() {
            if c.is_ascii_whitespace() {
                break;
            }
            end += 1;
        }
        let tok = String::from_utf8_lossy(&self.b[start..end.max(start + 1).min(self.b.len())]);
        syntax(format!(
            "syntax error at or near \"{tok}\" of jsonpath input"
        ))
    }

    fn lex_bareword(&mut self) -> String {
        let start = self.i;
        while let Some(c) = self.peek() {
            if is_ident_cont(c) {
                self.i += 1;
            } else {
                break;
            }
        }
        String::from_utf8_lossy(&self.b[start..self.i]).into_owned()
    }

    /// A double-quoted string with JSON escapes (cursor at the opening `"`).
    fn lex_quoted(&mut self) -> Result<String, JsonError> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(syntax("unexpected end of jsonpath input")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            let cp = self.lex_hex4()?;
                            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        }
                        _ => return Err(self.near_error()),
                    }
                    self.i += 1;
                }
                Some(c) if c < 0x80 => {
                    out.push(c as char);
                    self.i += 1;
                }
                Some(_) => {
                    // Multi-byte UTF-8: copy the whole char (input is valid UTF-8).
                    let start = self.i;
                    self.i += 1;
                    while let Some(c) = self.peek() {
                        if c < 0x80 || is_utf8_start(c) {
                            break;
                        }
                        self.i += 1;
                    }
                    out.push_str(&String::from_utf8_lossy(&self.b[start..self.i]));
                }
            }
        }
    }

    fn lex_hex4(&mut self) -> Result<u32, JsonError> {
        let mut v = 0u32;
        for _ in 0..4 {
            self.i += 1;
            let d = match self.peek() {
                Some(b @ b'0'..=b'9') => (b - b'0') as u32,
                Some(b @ b'a'..=b'f') => (b - b'a' + 10) as u32,
                Some(b @ b'A'..=b'F') => (b - b'A' + 10) as u32,
                _ => return Err(self.near_error()),
            };
            v = v * 16 + d;
        }
        Ok(v)
    }

    /// Scan a JSON-style number (no leading sign — unary minus is a separate
    /// token) and normalize through [`Numeric`].
    fn lex_number(&mut self) -> Result<Numeric, JsonError> {
        let start = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        if self.peek() == Some(b'.') && matches!(self.peek2(), Some(b'0'..=b'9')) {
            self.i += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let save = self.i;
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.i += 1;
                }
            } else {
                self.i = save; // not an exponent after all
            }
        }
        // A digit/letter/`.` immediately after a number is trailing junk (PG's
        // `5.double()` error): a number must be followed by an operator/accessor.
        if matches!(self.peek(), Some(c) if is_ident_start(c) || c == b'.') {
            let mut end = self.i;
            while matches!(self.b.get(end).copied(), Some(c) if is_ident_cont(c) || c == b'.') {
                end += 1;
            }
            let tok = String::from_utf8_lossy(&self.b[start..end]);
            return Err(syntax(format!(
                "trailing junk after numeric literal at or near \"{tok}\" of jsonpath input"
            )));
        }
        let text = String::from_utf8_lossy(&self.b[start..self.i]);
        Numeric::parse(&text).map_err(|_| self.near_error())
    }
}

fn is_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic() || c >= 0x80
}

fn is_ident_cont(c: u8) -> bool {
    c == b'_' || c == b'$' || c.is_ascii_alphanumeric() || c >= 0x80
}

fn is_utf8_start(c: u8) -> bool {
    (c & 0xC0) != 0x80
}

// ===========================================================================
// Parser (Pratt over the token stream)
// ===========================================================================

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// The `at or near "<tok>"` / `at end` syntax error for the current cursor.
    fn err_here(&self) -> JsonError {
        match self.peek() {
            None => syntax("syntax error at end of jsonpath input"),
            Some(t) => syntax(format!(
                "syntax error at or near \"{}\" of jsonpath input",
                tok_text(t)
            )),
        }
    }

    fn expect(&mut self, t: &Tok) -> Result<(), JsonError> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.err_here())
        }
    }

    fn enter(&mut self) -> Result<(), JsonError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(err("54001", "stack depth limit exceeded"));
        }
        Ok(())
    }

    // ---- expression grammar (lowest precedence first) --------------------

    fn parse_expr(&mut self) -> Result<Node, JsonError> {
        self.enter()?;
        let n = self.parse_or();
        self.depth -= 1;
        n
    }

    fn parse_or(&mut self) -> Result<Node, JsonError> {
        let mut left = self.parse_and()?;
        while self.eat(&Tok::OrOr) {
            let right = self.parse_and()?;
            left = Node::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Node, JsonError> {
        let mut left = self.parse_not()?;
        while self.eat(&Tok::AndAnd) {
            let right = self.parse_not()?;
            left = Node::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Node, JsonError> {
        if self.eat(&Tok::Bang) {
            // NOT binds looser than comparison: `!(@ == 5)`.
            let inner = self.parse_not()?;
            return Ok(Node::Not(Box::new(inner)));
        }
        self.parse_cmp()
    }

    fn parse_cmp(&mut self) -> Result<Node, JsonError> {
        let left = self.parse_add()?;
        // Comparison / string predicates are non-associative — at most one.
        let node = if let Some(op) = self.peek_cmp() {
            self.pos += 1;
            let right = self.parse_add()?;
            Node::Compare {
                op,
                left: Box::new(left),
                right: Box::new(right),
            }
        } else if self.peek_ident_ci("starts") {
            self.pos += 1;
            if !self.eat_ident_ci("with") {
                return Err(self.err_here());
            }
            let prefix = self.parse_add()?;
            Node::StartsWith {
                operand: Box::new(left),
                prefix: Box::new(prefix),
            }
        } else if self.eat_ident_ci("like_regex") {
            let pattern = match self.next() {
                Some(Tok::Str(s)) => s,
                _ => return Err(self.err_here()),
            };
            let raw = if self.eat_ident_ci("flag") {
                match self.next() {
                    Some(Tok::Str(s)) => s,
                    _ => return Err(self.err_here()),
                }
            } else {
                String::new()
            };
            // PG validates the flags and compiles the pattern while *parsing*
            // the path, so all three of these are errors on the cast rather
            // than on any row — a path over an empty array still raises them.
            let flags = crate::text::LikeRegexFlags::parse(&raw).map_err(|c| JsonError {
                sqlstate: SYNTAX_ERROR,
                message: "invalid input syntax for type jsonpath".to_string(),
                detail: Some(format!(
                    "Unrecognized flag character \"{c}\" in LIKE_REGEX predicate."
                )),
            })?;
            // `q` escapes the whole pattern, which makes expanded mode moot, so
            // PG only rejects `x` when `q` is absent.
            if flags.wspace && !flags.quote {
                return Err(err(
                    FEATURE_NOT_SUPPORTED,
                    "XQuery \"x\" flag (expanded regular expressions) is not implemented",
                ));
            }
            crate::text::like_regex_compile(&pattern, flags).map_err(text_err)?;
            Node::LikeRegex {
                operand: Box::new(left),
                pattern,
                flags,
            }
        } else {
            left
        };
        // Postfix `is unknown`.
        if self.peek_ident_ci("is") {
            self.pos += 1;
            if !self.eat_ident_ci("unknown") {
                return Err(self.err_here());
            }
            return Ok(Node::IsUnknown(Box::new(node)));
        }
        Ok(node)
    }

    fn parse_add(&mut self) -> Result<Node, JsonError> {
        let mut left = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => ArithOp::Add,
                Some(Tok::Minus) => ArithOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_mul()?;
            left = Node::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_mul(&mut self) -> Result<Node, JsonError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => ArithOp::Mul,
                Some(Tok::Slash) => ArithOp::Div,
                Some(Tok::Percent) => ArithOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_unary()?;
            left = Node::Arith {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Node, JsonError> {
        if self.eat(&Tok::Plus) {
            return self.parse_unary();
        }
        if self.eat(&Tok::Minus) {
            let operand = self.parse_unary()?;
            return Ok(Node::Unary {
                neg: true,
                operand: Box::new(operand),
            });
        }
        self.parse_postfix()
    }

    /// A primary followed by any run of accessor steps.
    fn parse_postfix(&mut self) -> Result<Node, JsonError> {
        let mut node = self.parse_primary()?;
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.pos += 1;
                    node = self.parse_dot_accessor(node)?;
                }
                Some(Tok::LBracket) => {
                    node = self.parse_bracket(node)?;
                }
                Some(Tok::Question) => {
                    self.pos += 1;
                    self.expect(&Tok::LParen)?;
                    self.enter()?;
                    let pred = self.parse_or()?;
                    self.depth -= 1;
                    self.expect(&Tok::RParen)?;
                    node = Node::Accessor {
                        base: Box::new(node),
                        step: Accessor::Filter(Box::new(pred)),
                    };
                }
                _ => break,
            }
        }
        Ok(node)
    }

    /// Parse whatever follows a `.`: `*`, `**{..}`, a method call, or a key.
    fn parse_dot_accessor(&mut self, base: Node) -> Result<Node, JsonError> {
        let step = match self.peek().cloned() {
            Some(Tok::Star) => {
                self.pos += 1;
                Accessor::WildcardMember
            }
            Some(Tok::StarStar) => {
                self.pos += 1;
                let (lo, hi) = self.parse_level_range()?;
                Accessor::Recursive(lo, hi)
            }
            Some(Tok::Ident(name)) => {
                // A method (`name ( )`) or a bareword object key.
                if let Some(m) = method_from_name(&name)
                    && self.toks.get(self.pos + 1) == Some(&Tok::LParen)
                {
                    self.pos += 2; // ident + '('
                    self.expect(&Tok::RParen)?;
                    Accessor::Method(m)
                } else {
                    self.pos += 1;
                    Accessor::Key(name)
                }
            }
            Some(Tok::Str(key)) => {
                self.pos += 1;
                Accessor::Key(key)
            }
            _ => return Err(self.err_here()),
        };
        Ok(Node::Accessor {
            base: Box::new(base),
            step,
        })
    }

    /// The optional `{lo}` / `{lo to hi}` level range after `.**`.
    fn parse_level_range(&mut self) -> Result<(Option<u32>, Option<u32>), JsonError> {
        if !self.eat(&Tok::LBrace) {
            return Ok((None, None));
        }
        let lo = self.parse_u32()?;
        let hi = if self.eat_ident_ci("to") {
            Some(self.parse_u32()?)
        } else {
            Some(lo)
        };
        self.expect(&Tok::RBrace)?;
        Ok((Some(lo), hi))
    }

    fn parse_u32(&mut self) -> Result<u32, JsonError> {
        match self.next() {
            Some(Tok::Num(n)) => n
                .to_i128()
                .filter(|v| *v >= 0 && *v <= u32::MAX as i128)
                .map(|v| v as u32)
                .ok_or_else(|| syntax("jsonpath level out of range")),
            _ => Err(self.err_here()),
        }
    }

    fn parse_bracket(&mut self, base: Node) -> Result<Node, JsonError> {
        self.expect(&Tok::LBracket)?;
        if self.eat(&Tok::Star) {
            self.expect(&Tok::RBracket)?;
            return Ok(Node::Accessor {
                base: Box::new(base),
                step: Accessor::WildcardArray,
            });
        }
        let mut subs = Vec::new();
        loop {
            self.enter()?;
            let from = self.parse_or()?;
            self.depth -= 1;
            if self.eat_ident_ci("to") {
                self.enter()?;
                let to = self.parse_or()?;
                self.depth -= 1;
                subs.push(Subscript::Range(from, to));
            } else {
                subs.push(Subscript::Index(from));
            }
            if self.eat(&Tok::Comma) {
                continue;
            }
            break;
        }
        self.expect(&Tok::RBracket)?;
        Ok(Node::Accessor {
            base: Box::new(base),
            step: Accessor::Subscript(subs),
        })
    }

    fn parse_primary(&mut self) -> Result<Node, JsonError> {
        // Peek first so a non-primary token (e.g. a stray `)`) is still current
        // for `err_here` — advancing only on a matched arm, PG's cursor position.
        match self.peek().cloned() {
            Some(Tok::Dollar) => {
                self.pos += 1;
                Ok(Node::Root)
            }
            Some(Tok::At) => {
                self.pos += 1;
                Ok(Node::Current)
            }
            Some(Tok::Var(name)) => {
                self.pos += 1;
                Ok(Node::Var(name))
            }
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Ok(Node::LitNum(n))
            }
            Some(Tok::Str(s)) => {
                self.pos += 1;
                Ok(Node::LitStr(s))
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                self.enter()?;
                let inner = self.parse_or()?;
                self.depth -= 1;
                self.expect(&Tok::RParen)?;
                Ok(inner)
            }
            Some(Tok::Ident(name)) => match name.to_ascii_lowercase().as_str() {
                "true" => {
                    self.pos += 1;
                    Ok(Node::LitBool(true))
                }
                "false" => {
                    self.pos += 1;
                    Ok(Node::LitBool(false))
                }
                "null" => {
                    self.pos += 1;
                    Ok(Node::LitNull)
                }
                "last" => {
                    self.pos += 1;
                    Ok(Node::Last)
                }
                "exists" => {
                    self.pos += 1;
                    self.expect(&Tok::LParen)?;
                    self.enter()?;
                    let inner = self.parse_or()?;
                    self.depth -= 1;
                    self.expect(&Tok::RParen)?;
                    Ok(Node::Exists(Box::new(inner)))
                }
                _ => Err(syntax(format!(
                    "syntax error at or near \"{name}\" of jsonpath input"
                ))),
            },
            _ => Err(self.err_here()),
        }
    }

    fn peek_cmp(&self) -> Option<CmpOp> {
        Some(match self.peek()? {
            Tok::Eq => CmpOp::Eq,
            Tok::Ne => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            _ => return None,
        })
    }

    fn peek_ident_ci(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_ident_ci(&mut self, kw: &str) -> bool {
        if self.peek_ident_ci(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
}

fn method_from_name(name: &str) -> Option<Method> {
    Some(match name.to_ascii_lowercase().as_str() {
        "size" => Method::Size,
        "type" => Method::Type,
        "double" => Method::Double,
        "abs" => Method::Abs,
        "floor" => Method::Floor,
        "ceiling" => Method::Ceiling,
        "keyvalue" => Method::KeyValue,
        _ => return None,
    })
}

/// Best-effort token text for a `syntax error at or near "..."` message.
fn tok_text(t: &Tok) -> String {
    match t {
        Tok::Dollar => "$".into(),
        Tok::At => "@".into(),
        Tok::Dot => ".".into(),
        Tok::Star => "*".into(),
        Tok::StarStar => "**".into(),
        Tok::LParen => "(".into(),
        Tok::RParen => ")".into(),
        Tok::LBracket => "[".into(),
        Tok::RBracket => "]".into(),
        Tok::LBrace => "{".into(),
        Tok::RBrace => "}".into(),
        Tok::Comma => ",".into(),
        Tok::Question => "?".into(),
        Tok::Bang => "!".into(),
        Tok::Plus => "+".into(),
        Tok::Minus => "-".into(),
        Tok::Slash => "/".into(),
        Tok::Percent => "%".into(),
        Tok::Eq => "==".into(),
        Tok::Ne => "!=".into(),
        Tok::Lt => "<".into(),
        Tok::Le => "<=".into(),
        Tok::Gt => ">".into(),
        Tok::Ge => ">=".into(),
        Tok::AndAnd => "&&".into(),
        Tok::OrOr => "||".into(),
        Tok::Num(n) => n.to_display(),
        Tok::Str(s) => s.clone(),
        Tok::Ident(s) => s.clone(),
        Tok::Var(s) => format!("${s}"),
    }
}

// ---------------------------------------------------------------------------
// Input entry point
// ---------------------------------------------------------------------------

/// `jsonpath_in`: parse a `jsonpath` text literal into a [`JsonPath`] program.
pub fn jsonpath_in(s: &str) -> Result<JsonPath, JsonError> {
    let toks = Lexer::new(s).tokenize()?;
    let mut p = Parser {
        toks,
        pos: 0,
        depth: 0,
    };
    // Optional leading `strict` / `lax` mode word (default lax).
    let strict = if p.eat_ident_ci("strict") {
        true
    } else {
        p.eat_ident_ci("lax");
        false
    };
    if p.pos >= p.toks.len() {
        return Err(syntax("syntax error at end of jsonpath input"));
    }
    let expr = p.parse_expr()?;
    if p.pos != p.toks.len() {
        return Err(p.err_here());
    }
    Ok(JsonPath { strict, expr })
}

// ===========================================================================
// Output (`jsonpath_out`)
// ===========================================================================

/// `jsonpath_out`: render the canonical text form. Keys are always double-quoted
/// and binary arithmetic is fully parenthesized, matching PG's printout.
pub fn format(p: &JsonPath) -> String {
    let mut out = String::new();
    if p.strict {
        out.push_str("strict ");
    }
    // The root call requests brackets, so a top-level binary op (`2 + 3` →
    // `(2 + 3)`) is wrapped, matching PG; primaries ignore the request.
    write_node(&mut out, &p.expr, true);
    out
}

// --- storage codec ----------------------------------------------------------
//
// A stored jsonpath must decode without re-running the parser. Re-parsing the
// canonical text looks safe but is not: [`format`] adds parentheses around
// equal-priority sub-expressions, so a path accepted just under [`MAX_DEPTH`]
// can come back deeper than it went in, and any tightening of the parser (as
// happened for `like_regex` flags) retroactively makes older stored values
// unreadable. `tsquery` avoids the same trap the same way.

const B_ROOT: u8 = 0;
const B_CURRENT: u8 = 1;
const B_LAST: u8 = 2;
const B_VAR: u8 = 3;
const B_NUM: u8 = 4;
const B_STR: u8 = 5;
const B_BOOL: u8 = 6;
const B_NULL: u8 = 7;
const B_ACCESSOR: u8 = 8;
const B_UNARY: u8 = 9;
const B_ARITH: u8 = 10;
const B_AND: u8 = 11;
const B_OR: u8 = 12;
const B_NOT: u8 = 13;
const B_COMPARE: u8 = 14;
const B_EXISTS: u8 = 15;
const B_STARTS_WITH: u8 = 16;
const B_LIKE_REGEX: u8 = 17;
const B_IS_UNKNOWN: u8 = 18;

const A_KEY: u8 = 0;
const A_WILDCARD_MEMBER: u8 = 1;
const A_WILDCARD_ARRAY: u8 = 2;
const A_RECURSIVE: u8 = 3;
const A_SUBSCRIPT: u8 = 4;
const A_METHOD: u8 = 5;
const A_FILTER: u8 = 6;

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn get_str(b: &[u8], i: &mut usize) -> Option<String> {
    let len = u32::from_le_bytes(b.get(*i..*i + 4)?.try_into().ok()?) as usize;
    *i += 4;
    let s = std::str::from_utf8(b.get(*i..*i + len)?).ok()?.to_string();
    *i += len;
    Some(s)
}

fn put_opt_u32(out: &mut Vec<u8>, v: Option<u32>) {
    match v {
        None => out.push(0),
        Some(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
}

fn get_opt_u32(b: &[u8], i: &mut usize) -> Option<Option<u32>> {
    let tag = *b.get(*i)?;
    *i += 1;
    if tag == 0 {
        return Some(None);
    }
    let n = u32::from_le_bytes(b.get(*i..*i + 4)?.try_into().ok()?);
    *i += 4;
    Some(Some(n))
}

/// Serialize a path for storage, in prefix order. See the note above for why
/// the canonical text form is not used.
pub fn encode(p: &JsonPath) -> Vec<u8> {
    fn put_accessor(a: &Accessor, out: &mut Vec<u8>) {
        match a {
            Accessor::Key(k) => {
                out.push(A_KEY);
                put_str(out, k);
            }
            Accessor::WildcardMember => out.push(A_WILDCARD_MEMBER),
            Accessor::WildcardArray => out.push(A_WILDCARD_ARRAY),
            Accessor::Recursive(lo, hi) => {
                out.push(A_RECURSIVE);
                put_opt_u32(out, *lo);
                put_opt_u32(out, *hi);
            }
            Accessor::Subscript(subs) => {
                out.push(A_SUBSCRIPT);
                out.extend_from_slice(&(subs.len() as u32).to_le_bytes());
                for s in subs {
                    match s {
                        Subscript::Index(n) => {
                            out.push(0);
                            put(n, out);
                        }
                        Subscript::Range(lo, hi) => {
                            out.push(1);
                            put(lo, out);
                            put(hi, out);
                        }
                    }
                }
            }
            Accessor::Method(m) => {
                out.push(A_METHOD);
                // Spelled out rather than `as u8` so reordering the enum cannot
                // silently reinterpret already-stored data.
                out.push(match m {
                    Method::Size => 0,
                    Method::Type => 1,
                    Method::Double => 2,
                    Method::Abs => 3,
                    Method::Floor => 4,
                    Method::Ceiling => 5,
                    Method::KeyValue => 6,
                });
            }
            Accessor::Filter(n) => {
                out.push(A_FILTER);
                put(n, out);
            }
        }
    }
    fn put(n: &Node, out: &mut Vec<u8>) {
        match n {
            Node::Root => out.push(B_ROOT),
            Node::Current => out.push(B_CURRENT),
            Node::Last => out.push(B_LAST),
            Node::Var(v) => {
                out.push(B_VAR);
                put_str(out, v);
            }
            // `Numeric`'s text form is exact, and `datum` already relies on that
            // for a stored `numeric`.
            Node::LitNum(v) => {
                out.push(B_NUM);
                put_str(out, &v.to_display());
            }
            Node::LitStr(s) => {
                out.push(B_STR);
                put_str(out, s);
            }
            Node::LitBool(b) => {
                out.push(B_BOOL);
                out.push(u8::from(*b));
            }
            Node::LitNull => out.push(B_NULL),
            Node::Accessor { base, step } => {
                out.push(B_ACCESSOR);
                put(base, out);
                put_accessor(step, out);
            }
            Node::Unary { neg, operand } => {
                out.push(B_UNARY);
                out.push(u8::from(*neg));
                put(operand, out);
            }
            Node::Arith { op, left, right } => {
                out.push(B_ARITH);
                out.push(match op {
                    ArithOp::Add => 0,
                    ArithOp::Sub => 1,
                    ArithOp::Mul => 2,
                    ArithOp::Div => 3,
                    ArithOp::Mod => 4,
                });
                put(left, out);
                put(right, out);
            }
            Node::And(l, r) => {
                out.push(B_AND);
                put(l, out);
                put(r, out);
            }
            Node::Or(l, r) => {
                out.push(B_OR);
                put(l, out);
                put(r, out);
            }
            Node::Not(x) => {
                out.push(B_NOT);
                put(x, out);
            }
            Node::Compare { op, left, right } => {
                out.push(B_COMPARE);
                out.push(match op {
                    CmpOp::Eq => 0,
                    CmpOp::Ne => 1,
                    CmpOp::Lt => 2,
                    CmpOp::Le => 3,
                    CmpOp::Gt => 4,
                    CmpOp::Ge => 5,
                });
                put(left, out);
                put(right, out);
            }
            Node::Exists(x) => {
                out.push(B_EXISTS);
                put(x, out);
            }
            Node::StartsWith { operand, prefix } => {
                out.push(B_STARTS_WITH);
                put(operand, out);
                put(prefix, out);
            }
            Node::LikeRegex {
                operand,
                pattern,
                flags,
            } => {
                out.push(B_LIKE_REGEX);
                put(operand, out);
                put_str(out, pattern);
                put_str(out, &flags.canonical());
            }
            Node::IsUnknown(x) => {
                out.push(B_IS_UNKNOWN);
                put(x, out);
            }
        }
    }
    let mut out = vec![u8::from(p.strict)];
    put(&p.expr, &mut out);
    out
}

/// Inverse of [`encode`]. `None` if the bytes are malformed or nest deeper than
/// [`MAX_DEPTH`] — both impossible for a datum this build wrote, but checked so
/// a corrupt page cannot overflow the stack.
pub fn decode(bytes: &[u8]) -> Option<JsonPath> {
    fn get_accessor(b: &[u8], i: &mut usize, depth: usize) -> Option<Accessor> {
        let tag = *b.get(*i)?;
        *i += 1;
        Some(match tag {
            A_KEY => Accessor::Key(get_str(b, i)?),
            A_WILDCARD_MEMBER => Accessor::WildcardMember,
            A_WILDCARD_ARRAY => Accessor::WildcardArray,
            A_RECURSIVE => Accessor::Recursive(get_opt_u32(b, i)?, get_opt_u32(b, i)?),
            A_SUBSCRIPT => {
                let n = u32::from_le_bytes(b.get(*i..*i + 4)?.try_into().ok()?) as usize;
                *i += 4;
                // A length header cannot be trusted from a corrupt page, so the
                // elements themselves have to run out first.
                let mut subs = Vec::new();
                for _ in 0..n {
                    let kind = *b.get(*i)?;
                    *i += 1;
                    subs.push(match kind {
                        0 => Subscript::Index(get(b, i, depth + 1)?),
                        1 => Subscript::Range(get(b, i, depth + 1)?, get(b, i, depth + 1)?),
                        _ => return None,
                    });
                }
                Accessor::Subscript(subs)
            }
            A_METHOD => {
                let m = match *b.get(*i)? {
                    0 => Method::Size,
                    1 => Method::Type,
                    2 => Method::Double,
                    3 => Method::Abs,
                    4 => Method::Floor,
                    5 => Method::Ceiling,
                    6 => Method::KeyValue,
                    _ => return None,
                };
                *i += 1;
                Accessor::Method(m)
            }
            A_FILTER => Accessor::Filter(Box::new(get(b, i, depth + 1)?)),
            _ => return None,
        })
    }
    fn get(b: &[u8], i: &mut usize, depth: usize) -> Option<Node> {
        if depth > MAX_DEPTH {
            return None;
        }
        let tag = *b.get(*i)?;
        *i += 1;
        Some(match tag {
            B_ROOT => Node::Root,
            B_CURRENT => Node::Current,
            B_LAST => Node::Last,
            B_VAR => Node::Var(get_str(b, i)?),
            B_NUM => Node::LitNum(Numeric::parse(&get_str(b, i)?).ok()?),
            B_STR => Node::LitStr(get_str(b, i)?),
            B_BOOL => {
                let v = *b.get(*i)? != 0;
                *i += 1;
                Node::LitBool(v)
            }
            B_NULL => Node::LitNull,
            B_ACCESSOR => Node::Accessor {
                base: Box::new(get(b, i, depth + 1)?),
                step: get_accessor(b, i, depth)?,
            },
            B_UNARY => {
                let neg = *b.get(*i)? != 0;
                *i += 1;
                Node::Unary {
                    neg,
                    operand: Box::new(get(b, i, depth + 1)?),
                }
            }
            B_ARITH => {
                let op = match *b.get(*i)? {
                    0 => ArithOp::Add,
                    1 => ArithOp::Sub,
                    2 => ArithOp::Mul,
                    3 => ArithOp::Div,
                    4 => ArithOp::Mod,
                    _ => return None,
                };
                *i += 1;
                Node::Arith {
                    op,
                    left: Box::new(get(b, i, depth + 1)?),
                    right: Box::new(get(b, i, depth + 1)?),
                }
            }
            B_AND => Node::And(
                Box::new(get(b, i, depth + 1)?),
                Box::new(get(b, i, depth + 1)?),
            ),
            B_OR => Node::Or(
                Box::new(get(b, i, depth + 1)?),
                Box::new(get(b, i, depth + 1)?),
            ),
            B_NOT => Node::Not(Box::new(get(b, i, depth + 1)?)),
            B_COMPARE => {
                let op = match *b.get(*i)? {
                    0 => CmpOp::Eq,
                    1 => CmpOp::Ne,
                    2 => CmpOp::Lt,
                    3 => CmpOp::Le,
                    4 => CmpOp::Gt,
                    5 => CmpOp::Ge,
                    _ => return None,
                };
                *i += 1;
                Node::Compare {
                    op,
                    left: Box::new(get(b, i, depth + 1)?),
                    right: Box::new(get(b, i, depth + 1)?),
                }
            }
            B_EXISTS => Node::Exists(Box::new(get(b, i, depth + 1)?)),
            B_STARTS_WITH => Node::StartsWith {
                operand: Box::new(get(b, i, depth + 1)?),
                prefix: Box::new(get(b, i, depth + 1)?),
            },
            B_LIKE_REGEX => Node::LikeRegex {
                operand: Box::new(get(b, i, depth + 1)?),
                pattern: get_str(b, i)?,
                flags: crate::text::LikeRegexFlags::parse(&get_str(b, i)?).ok()?,
            },
            B_IS_UNKNOWN => Node::IsUnknown(Box::new(get(b, i, depth + 1)?)),
            _ => return None,
        })
    }
    let strict = *bytes.first()? != 0;
    let mut i = 1;
    let expr = get(bytes, &mut i, 0)?;
    (i == bytes.len()).then_some(JsonPath { strict, expr })
}

/// Operator priority (PG's `operationPriority`): a child is parenthesized when
/// its priority is `<=` its parent's, so lower-priority (looser-binding) or
/// equal-priority sub-expressions print with explicit grouping.
fn prio(node: &Node) -> u8 {
    match node {
        Node::Or(..) => 0,
        Node::And(..) => 1,
        Node::Not(_) => 2,
        Node::Compare { .. }
        | Node::StartsWith { .. }
        | Node::LikeRegex { .. }
        | Node::IsUnknown(_) => 3,
        Node::Arith {
            op: ArithOp::Add | ArithOp::Sub,
            ..
        } => 4,
        Node::Arith { .. } => 5,
        Node::Unary { .. } => 6,
        _ => 7,
    }
}

/// `brackets` wraps this node in parens if it is an operator node; primaries and
/// accessor chains ignore it.
fn write_node(out: &mut String, node: &Node, brackets: bool) {
    match node {
        Node::Root => out.push('$'),
        Node::Current => out.push('@'),
        Node::Last => out.push_str("last"),
        Node::Var(name) => {
            out.push('$');
            write_json_string(out, name);
        }
        Node::LitNum(n) => out.push_str(&n.to_display()),
        Node::LitStr(s) => write_json_string(out, s),
        Node::LitBool(b) => out.push_str(if *b { "true" } else { "false" }),
        Node::LitNull => out.push_str("null"),
        Node::Accessor { base, step } => {
            // Wrap the base only when it is an operator node (`($.a + 1).size()`).
            write_node(out, base, true);
            write_step(out, step);
        }
        Node::Unary { neg, operand } => {
            if brackets {
                out.push('(');
            }
            if *neg {
                out.push('-');
            }
            write_node(out, operand, prio(operand) <= 6);
            if brackets {
                out.push(')');
            }
        }
        Node::Arith { op, left, right } => {
            let sym = match op {
                ArithOp::Add => "+",
                ArithOp::Sub => "-",
                ArithOp::Mul => "*",
                ArithOp::Div => "/",
                ArithOp::Mod => "%",
            };
            write_binary(out, brackets, left, sym, right, prio(node));
        }
        Node::And(l, r) => write_binary(out, brackets, l, "&&", r, prio(node)),
        Node::Or(l, r) => write_binary(out, brackets, l, "||", r, prio(node)),
        Node::Not(inner) => {
            out.push_str("!(");
            write_node(out, inner, false);
            out.push(')');
        }
        Node::Compare { op, left, right } => {
            write_binary(out, brackets, left, op.symbol(), right, prio(node));
        }
        Node::Exists(inner) => {
            out.push_str("exists (");
            write_node(out, inner, false);
            out.push(')');
        }
        Node::StartsWith { operand, prefix } => {
            let sp = prio(node);
            if brackets {
                out.push('(');
            }
            write_node(out, operand, prio(operand) <= sp);
            out.push_str(" starts with ");
            write_node(out, prefix, prio(prefix) <= sp);
            if brackets {
                out.push(')');
            }
        }
        Node::LikeRegex {
            operand,
            pattern,
            flags,
        } => {
            let sp = prio(node);
            if brackets {
                out.push('(');
            }
            write_node(out, operand, prio(operand) <= sp);
            out.push_str(" like_regex ");
            write_json_string(out, pattern);
            // Re-emitted from the parsed set, so the spelling is canonical
            // rather than whatever the user wrote.
            if !flags.is_empty() {
                out.push_str(" flag ");
                write_json_string(out, &flags.canonical());
            }
            if brackets {
                out.push(')');
            }
        }
        Node::IsUnknown(inner) => {
            out.push('(');
            write_node(out, inner, false);
            out.push_str(") is unknown");
        }
    }
}

fn write_binary(
    out: &mut String,
    brackets: bool,
    left: &Node,
    sym: &str,
    right: &Node,
    self_prio: u8,
) {
    if brackets {
        out.push('(');
    }
    write_node(out, left, prio(left) <= self_prio);
    out.push(' ');
    out.push_str(sym);
    out.push(' ');
    write_node(out, right, prio(right) <= self_prio);
    if brackets {
        out.push(')');
    }
}

fn write_step(out: &mut String, step: &Accessor) {
    match step {
        Accessor::Key(k) => {
            out.push('.');
            write_json_string(out, k);
        }
        Accessor::WildcardMember => out.push_str(".*"),
        Accessor::WildcardArray => out.push_str("[*]"),
        Accessor::Recursive(lo, hi) => {
            out.push_str(".**");
            if let Some(lo) = lo {
                out.push('{');
                out.push_str(&lo.to_string());
                if let Some(hi) = hi
                    && hi != lo
                {
                    out.push_str(" to ");
                    out.push_str(&hi.to_string());
                }
                out.push('}');
            }
        }
        Accessor::Subscript(subs) => {
            out.push('[');
            for (i, s) in subs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match s {
                    Subscript::Index(n) => write_node(out, n, false),
                    Subscript::Range(a, b) => {
                        write_node(out, a, false);
                        out.push_str(" to ");
                        write_node(out, b, false);
                    }
                }
            }
            out.push(']');
        }
        Accessor::Method(m) => {
            out.push('.');
            out.push_str(m.name());
            out.push_str("()");
        }
        Accessor::Filter(pred) => {
            out.push_str("?(");
            write_node(out, pred, false);
            out.push(')');
        }
    }
}

/// Escape a string as a jsonpath double-quoted token (same rules as JSON).
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ===========================================================================
// Evaluation
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Debug)]
enum Ternary {
    True,
    False,
    Unknown,
}

impl Ternary {
    fn from_bool(b: bool) -> Ternary {
        if b { Ternary::True } else { Ternary::False }
    }
}

struct Eval<'a> {
    root: &'a Jsonb,
    vars: Option<&'a Jsonb>,
    strict: bool,
}

impl Eval<'_> {
    /// Evaluate a value/path node against `current` (`@`), producing a sequence.
    /// `last` is the index of the final array element when inside a subscript.
    fn seq(
        &self,
        node: &Node,
        current: &Jsonb,
        last: Option<i64>,
    ) -> Result<Vec<Jsonb>, JsonError> {
        match node {
            Node::Root => Ok(vec![self.root.clone()]),
            Node::Current => Ok(vec![current.clone()]),
            Node::Last => match last {
                Some(n) => Ok(vec![Jsonb::Number(Numeric::from_i128(n as i128))]),
                None => Err(syntax("LAST is allowed only in array subscripts")),
            },
            Node::Var(name) => Ok(vec![self.lookup_var(name)?]),
            Node::LitNum(n) => Ok(vec![Jsonb::Number(n.clone())]),
            Node::LitStr(s) => Ok(vec![Jsonb::String(s.clone())]),
            Node::LitBool(b) => Ok(vec![Jsonb::Bool(*b)]),
            Node::LitNull => Ok(vec![Jsonb::Null]),
            Node::Accessor { base, step } => {
                let items = self.seq(base, current, last)?;
                let mut out = Vec::new();
                for item in &items {
                    self.apply_step(step, item, &mut out)?;
                }
                Ok(out)
            }
            Node::Unary { neg, operand } => {
                let sym = if *neg { "-" } else { "+" };
                let items = self.seq(operand, current, last)?;
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    let Jsonb::Number(n) = &it else {
                        return Err(err(
                            SQL_JSON_NUMBER_NOT_FOUND,
                            format!(
                                "operand of unary jsonpath operator {sym} is not a numeric value"
                            ),
                        ));
                    };
                    out.push(Jsonb::Number(if *neg { n.neg() } else { n.clone() }));
                }
                Ok(out)
            }
            Node::Arith { op, left, right } => {
                let sym = op.symbol();
                let l = self.single_number(left, current, "left", sym)?;
                let r = self.single_number(right, current, "right", sym)?;
                Ok(vec![Jsonb::Number(arith(*op, &l, &r)?)])
            }
            // A predicate used in value position (top-level query of a predicate)
            // yields a single boolean item; Unknown serializes as JSON null.
            _ if is_predicate(node) => {
                let t = self.pred(node, current)?;
                Ok(vec![match t {
                    Ternary::True => Jsonb::Bool(true),
                    Ternary::False => Jsonb::Bool(false),
                    Ternary::Unknown => Jsonb::Null,
                }])
            }
            _ => Err(syntax("unexpected jsonpath expression")),
        }
    }

    fn lookup_var(&self, name: &str) -> Result<Jsonb, JsonError> {
        if let Some(Jsonb::Object(pairs)) = self.vars
            && let Some((_, v)) = pairs.iter().find(|(k, _)| k == name)
        {
            return Ok(v.clone());
        }
        Err(err(
            UNDEFINED_OBJECT,
            format!("could not find jsonpath variable \"{name}\""),
        ))
    }

    /// Evaluate a binary-arithmetic `node` to exactly one numeric value (with lax
    /// array unwrap), raising PG's "not a single numeric value" error otherwise.
    /// `side` is "left"/"right" and `op` the operator symbol, for the message.
    fn single_number(
        &self,
        node: &Node,
        current: &Jsonb,
        side: &str,
        op: &str,
    ) -> Result<Numeric, JsonError> {
        let items = self.seq(node, current, None)?;
        let unwrapped = self.unwrap_for_arith(items);
        match unwrapped.as_slice() {
            [Jsonb::Number(n)] => Ok(n.clone()),
            _ => Err(err(
                SINGLETON_JSON_ITEM_REQUIRED,
                format!("{side} operand of jsonpath operator {op} is not a single numeric value"),
            )),
        }
    }

    /// In lax mode a single array operand of arithmetic is unwrapped to its
    /// elements; strict leaves it (so a non-singleton stays an error).
    fn unwrap_for_arith(&self, items: Vec<Jsonb>) -> Vec<Jsonb> {
        if !self.strict
            && items.len() == 1
            && let Jsonb::Array(a) = &items[0]
        {
            return a.clone();
        }
        items
    }

    /// Apply one accessor `step` to a single `item`, pushing results onto `out`.
    fn apply_step(
        &self,
        step: &Accessor,
        item: &Jsonb,
        out: &mut Vec<Jsonb>,
    ) -> Result<(), JsonError> {
        match step {
            Accessor::Key(k) => self.apply_key(k, item, out),
            Accessor::WildcardMember => self.apply_wildcard_member(item, out),
            Accessor::WildcardArray => self.apply_wildcard_array(item, out),
            Accessor::Recursive(lo, hi) => {
                self.apply_recursive(
                    item,
                    lo.unwrap_or(0),
                    hi.map(|h| h as i64).unwrap_or(i64::MAX),
                    0,
                    out,
                );
                Ok(())
            }
            Accessor::Subscript(subs) => self.apply_subscript(subs, item, out),
            Accessor::Method(m) => self.apply_method(*m, item, out),
            Accessor::Filter(pred) => self.apply_filter(pred, item, out),
        }
    }

    fn apply_key(&self, k: &str, item: &Jsonb, out: &mut Vec<Jsonb>) -> Result<(), JsonError> {
        match item {
            Jsonb::Object(pairs) => {
                if let Some((_, v)) = pairs.iter().find(|(key, _)| key == k) {
                    out.push(v.clone());
                    Ok(())
                } else if self.strict {
                    Err(err(
                        SQL_JSON_MEMBER_NOT_FOUND,
                        format!("JSON object does not contain key \"{k}\""),
                    ))
                } else {
                    Ok(())
                }
            }
            Jsonb::Array(a) if !self.strict => {
                // lax auto-unwrap: apply the member accessor to each element.
                for elem in a {
                    self.apply_key(k, elem, out)?;
                }
                Ok(())
            }
            _ if self.strict => Err(err(
                SQL_JSON_MEMBER_NOT_FOUND,
                "jsonpath member accessor can only be applied to an object",
            )),
            _ => Ok(()),
        }
    }

    fn apply_wildcard_member(&self, item: &Jsonb, out: &mut Vec<Jsonb>) -> Result<(), JsonError> {
        match item {
            Jsonb::Object(pairs) => {
                out.extend(pairs.iter().map(|(_, v)| v.clone()));
                Ok(())
            }
            Jsonb::Array(a) if !self.strict => {
                for elem in a {
                    self.apply_wildcard_member(elem, out)?;
                }
                Ok(())
            }
            _ if self.strict => Err(err(
                SQL_JSON_MEMBER_NOT_FOUND,
                "jsonpath wildcard member accessor can only be applied to an object",
            )),
            _ => Ok(()),
        }
    }

    fn apply_wildcard_array(&self, item: &Jsonb, out: &mut Vec<Jsonb>) -> Result<(), JsonError> {
        match item {
            Jsonb::Array(a) => {
                out.extend(a.iter().cloned());
                Ok(())
            }
            _ if self.strict => Err(err(
                SQL_JSON_ARRAY_NOT_FOUND,
                "jsonpath wildcard array accessor can only be applied to an array",
            )),
            // lax: a non-array is wrapped as a single-element array.
            _ => {
                out.push(item.clone());
                Ok(())
            }
        }
    }

    fn apply_recursive(&self, item: &Jsonb, lo: u32, hi: i64, depth: u32, out: &mut Vec<Jsonb>) {
        if depth as i64 <= hi && depth >= lo {
            out.push(item.clone());
        }
        if (depth as i64) >= hi {
            return;
        }
        match item {
            Jsonb::Array(a) => {
                for e in a {
                    self.apply_recursive(e, lo, hi, depth + 1, out);
                }
            }
            Jsonb::Object(pairs) => {
                for (_, v) in pairs {
                    self.apply_recursive(v, lo, hi, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    fn apply_subscript(
        &self,
        subs: &[Subscript],
        item: &Jsonb,
        out: &mut Vec<Jsonb>,
    ) -> Result<(), JsonError> {
        // Determine the array to index (lax wraps a non-array).
        let arr: Vec<Jsonb> = match item {
            Jsonb::Array(a) => a.clone(),
            _ if self.strict => {
                return Err(err(
                    SQL_JSON_ARRAY_NOT_FOUND,
                    "jsonpath array accessor can only be applied to an array",
                ));
            }
            _ => vec![item.clone()],
        };
        let len = arr.len() as i64;
        let last = Some(len - 1);
        for sub in subs {
            match sub {
                Subscript::Index(e) => {
                    let idx = self.single_index(e, item, last)?;
                    self.push_index(&arr, idx, out)?;
                }
                Subscript::Range(a, b) => {
                    let from = self.single_index(a, item, last)?;
                    let to = self.single_index(b, item, last)?;
                    for idx in from..=to {
                        self.push_index(&arr, idx, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn push_index(&self, arr: &[Jsonb], idx: i64, out: &mut Vec<Jsonb>) -> Result<(), JsonError> {
        if idx >= 0 && (idx as usize) < arr.len() {
            out.push(arr[idx as usize].clone());
            Ok(())
        } else if self.strict {
            Err(err(
                SQL_JSON_ARRAY_NOT_FOUND,
                "jsonpath array subscript is out of bounds",
            ))
        } else {
            Ok(())
        }
    }

    /// Evaluate a subscript index expression to a single integer (floor of a
    /// numeric), with `@` bound to the array's containing item and `last` set.
    fn single_index(
        &self,
        node: &Node,
        current: &Jsonb,
        last: Option<i64>,
    ) -> Result<i64, JsonError> {
        let items = self.seq(node, current, last)?;
        match items.as_slice() {
            [Jsonb::Number(n)] => n.floor().to_i128().map(|v| v as i64).ok_or_else(|| {
                err(
                    SQL_JSON_ARRAY_NOT_FOUND,
                    "jsonpath array subscript is out of integer range",
                )
            }),
            _ => Err(err(
                SQL_JSON_ARRAY_NOT_FOUND,
                "jsonpath array subscript is not a single numeric value",
            )),
        }
    }

    fn apply_method(&self, m: Method, item: &Jsonb, out: &mut Vec<Jsonb>) -> Result<(), JsonError> {
        // In lax mode these methods unwrap an array operand ONE level, applying
        // per element (`$.abs()` on `[1,-2]` → 1,2). `.size()`/`.type()` operate
        // on the container itself and never unwrap.
        if !self.strict
            && matches!(
                m,
                Method::Abs | Method::Floor | Method::Ceiling | Method::Double | Method::KeyValue
            )
            && let Jsonb::Array(a) = item
        {
            for elem in a {
                self.apply_method_scalar(m, elem, out)?;
            }
            return Ok(());
        }
        self.apply_method_scalar(m, item, out)
    }

    /// Apply an item method to a single (already-unwrapped) item.
    fn apply_method_scalar(
        &self,
        m: Method,
        item: &Jsonb,
        out: &mut Vec<Jsonb>,
    ) -> Result<(), JsonError> {
        match m {
            Method::Size => {
                let n = match item {
                    Jsonb::Array(a) => a.len() as i128,
                    // strict: `.size()` only applies to an array; lax: any
                    // non-array has "size" 1.
                    _ if self.strict => {
                        return Err(err(
                            SQL_JSON_ARRAY_NOT_FOUND,
                            "jsonpath item method .size() can only be applied to an array",
                        ));
                    }
                    _ => 1,
                };
                out.push(Jsonb::Number(Numeric::from_i128(n)));
                Ok(())
            }
            Method::Type => {
                out.push(Jsonb::String(type_name(item).to_string()));
                Ok(())
            }
            Method::Double => {
                let n = match item {
                    Jsonb::Number(n) => n.clone(),
                    Jsonb::String(s) => Numeric::parse(s.trim()).map_err(|_| {
                        err(
                            SQL_JSON_ITEM_METHOD,
                            format!(
                                "argument \"{s}\" of jsonpath item method .double() is invalid for type double precision"
                            ),
                        )
                    })?,
                    _ => {
                        return Err(err(
                            SQL_JSON_ITEM_METHOD,
                            "jsonpath item method .double() can only be applied to a string or numeric value",
                        ));
                    }
                };
                out.push(Jsonb::Number(n));
                Ok(())
            }
            Method::Abs => self.numeric_method(item, "abs", |n| n.abs(), out),
            Method::Floor => self.numeric_method(item, "floor", |n| n.floor(), out),
            Method::Ceiling => self.numeric_method(item, "ceiling", |n| n.ceil(), out),
            Method::KeyValue => match item {
                Jsonb::Object(pairs) => {
                    for (k, v) in pairs {
                        // jsonb canonical key order sorts shorter keys first, so
                        // `id` (2) < `key` (3) < `value` (5); build in that order.
                        //
                        // `id` is PG's byte offset of the containing object within
                        // the jsonb datum — reproducing it would need PG's on-disk
                        // binary layout, which this engine doesn't model (jsonb is
                        // a canonical tree). PG documents `id` as implementation-
                        // specific and not meaningful, so we always emit 0; this is
                        // correct for a top-level object and a known deviation for
                        // nested ones.
                        out.push(Jsonb::Object(vec![
                            ("id".to_string(), Jsonb::Number(Numeric::from_i128(0))),
                            ("key".to_string(), Jsonb::String(k.clone())),
                            ("value".to_string(), v.clone()),
                        ]));
                    }
                    Ok(())
                }
                _ => Err(err(
                    SQL_JSON_MEMBER_NOT_FOUND,
                    "jsonpath item method .keyvalue() can only be applied to an object",
                )),
            },
        }
    }

    fn numeric_method(
        &self,
        item: &Jsonb,
        name: &str,
        f: impl Fn(&Numeric) -> Numeric,
        out: &mut Vec<Jsonb>,
    ) -> Result<(), JsonError> {
        match item {
            Jsonb::Number(n) => {
                out.push(Jsonb::Number(f(n)));
                Ok(())
            }
            _ => Err(err(
                SQL_JSON_ITEM_METHOD,
                format!("jsonpath item method .{name}() can only be applied to a numeric value"),
            )),
        }
    }

    fn apply_filter(
        &self,
        pred: &Node,
        item: &Jsonb,
        out: &mut Vec<Jsonb>,
    ) -> Result<(), JsonError> {
        // lax: a filter unwraps an array operand, applying to each element.
        let candidates: Vec<&Jsonb> = match item {
            Jsonb::Array(a) if !self.strict => a.iter().collect(),
            _ => vec![item],
        };
        for cand in candidates {
            // Structural errors inside a filter predicate are suppressed (the
            // item just doesn't match); a missing-variable error propagates.
            match self.pred(pred, cand) {
                Ok(Ternary::True) => out.push(cand.clone()),
                Ok(_) => {}
                Err(e) if suppressible(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    // ---- predicates ------------------------------------------------------

    fn pred(&self, node: &Node, current: &Jsonb) -> Result<Ternary, JsonError> {
        match node {
            Node::And(l, r) => {
                let a = self.pred(l, current)?;
                let b = self.pred(r, current)?;
                Ok(match (a, b) {
                    (Ternary::False, _) | (_, Ternary::False) => Ternary::False,
                    (Ternary::True, Ternary::True) => Ternary::True,
                    _ => Ternary::Unknown,
                })
            }
            Node::Or(l, r) => {
                let a = self.pred(l, current)?;
                let b = self.pred(r, current)?;
                Ok(match (a, b) {
                    (Ternary::True, _) | (_, Ternary::True) => Ternary::True,
                    (Ternary::False, Ternary::False) => Ternary::False,
                    _ => Ternary::Unknown,
                })
            }
            Node::Not(inner) => Ok(match self.pred(inner, current)? {
                Ternary::True => Ternary::False,
                Ternary::False => Ternary::True,
                Ternary::Unknown => Ternary::Unknown,
            }),
            Node::Compare { op, left, right } => self.compare(*op, left, right, current),
            Node::StartsWith { operand, prefix } => self.starts_with(operand, prefix, current),
            Node::LikeRegex {
                operand,
                pattern,
                flags,
            } => self.like_regex(operand, pattern, *flags, current),
            Node::Exists(inner) => {
                // exists() suppresses structural errors → Unknown.
                match self.seq(inner, current, None) {
                    Ok(items) => Ok(Ternary::from_bool(!items.is_empty())),
                    Err(e) if suppressible(&e) => Ok(Ternary::Unknown),
                    Err(e) => Err(e),
                }
            }
            Node::IsUnknown(inner) => {
                let t = match self.pred(inner, current) {
                    Ok(t) => t,
                    Err(e) if suppressible(&e) => Ternary::Unknown,
                    Err(e) => return Err(e),
                };
                Ok(Ternary::from_bool(t == Ternary::Unknown))
            }
            _ => Err(err(
                SINGLETON_JSON_ITEM_REQUIRED,
                "single boolean result is expected",
            )),
        }
    }

    /// Existential comparison: true if any (lhs, rhs) pair satisfies `op`; a
    /// type-incompatible pair contributes Unknown; a structural error while
    /// evaluating an operand is suppressed to Unknown.
    fn compare(
        &self,
        op: CmpOp,
        left: &Node,
        right: &Node,
        current: &Jsonb,
    ) -> Result<Ternary, JsonError> {
        let (ls, rs) = match (
            self.seq(left, current, None),
            self.seq(right, current, None),
        ) {
            (Ok(l), Ok(r)) => (l, r),
            (Err(e), _) | (_, Err(e)) if suppressible(&e) => return Ok(Ternary::Unknown),
            (Err(e), _) | (_, Err(e)) => return Err(e),
        };
        let ls = self.unwrap_pred(ls);
        let rs = self.unwrap_pred(rs);
        let mut unknown = false;
        for l in &ls {
            for r in &rs {
                match compare_scalars(op, l, r) {
                    Ternary::True => return Ok(Ternary::True),
                    Ternary::Unknown => unknown = true,
                    Ternary::False => {}
                }
            }
        }
        Ok(if unknown {
            Ternary::Unknown
        } else {
            Ternary::False
        })
    }

    fn starts_with(
        &self,
        operand: &Node,
        prefix: &Node,
        current: &Jsonb,
    ) -> Result<Ternary, JsonError> {
        let ls = self.pred_operand(operand, current)?;
        let ps = self.pred_operand(prefix, current)?;
        let mut unknown = false;
        for l in &ls {
            for p in &ps {
                match (l, p) {
                    (Jsonb::String(s), Jsonb::String(pre)) => {
                        if s.starts_with(pre.as_str()) {
                            return Ok(Ternary::True);
                        }
                    }
                    _ => unknown = true,
                }
            }
        }
        Ok(if unknown {
            Ternary::Unknown
        } else {
            Ternary::False
        })
    }

    fn like_regex(
        &self,
        operand: &Node,
        pattern: &str,
        flags: crate::text::LikeRegexFlags,
        current: &Jsonb,
    ) -> Result<Ternary, JsonError> {
        let ls = self.pred_operand(operand, current)?;
        let mut unknown = false;
        for l in &ls {
            match l {
                // `like_regex` is XQuery-flavored, not POSIX: its flags differ
                // from the `~` operator's defaults, so it does not go through
                // `regex_match`.
                Jsonb::String(s) => match crate::text::like_regex_match(s, pattern, flags) {
                    Ok(true) => return Ok(Ternary::True),
                    Ok(false) => {}
                    // Unreachable: the parser compiled this same pattern under
                    // these same flags. Propagating rather than degrading to
                    // Unknown keeps any future drift between the two loud.
                    Err(e) => return Err(text_err(e)),
                },
                _ => unknown = true,
            }
        }
        Ok(if unknown {
            Ternary::Unknown
        } else {
            Ternary::False
        })
    }

    /// Evaluate a predicate operand to a sequence, suppressing structural errors
    /// to an empty sequence (so the predicate becomes False/Unknown, not an Err).
    fn pred_operand(&self, node: &Node, current: &Jsonb) -> Result<Vec<Jsonb>, JsonError> {
        match self.seq(node, current, None) {
            Ok(items) => Ok(self.unwrap_pred(items)),
            Err(e) if suppressible(&e) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// lax auto-unwrap of array operands for comparisons/string predicates.
    fn unwrap_pred(&self, items: Vec<Jsonb>) -> Vec<Jsonb> {
        if self.strict {
            return items;
        }
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            match it {
                Jsonb::Array(a) => out.extend(a),
                other => out.push(other),
            }
        }
        out
    }
}

fn is_predicate(node: &Node) -> bool {
    matches!(
        node,
        Node::And(..)
            | Node::Or(..)
            | Node::Not(_)
            | Node::Compare { .. }
            | Node::Exists(_)
            | Node::StartsWith { .. }
            | Node::LikeRegex { .. }
            | Node::IsUnknown(_)
    )
}

fn arith(op: ArithOp, a: &Numeric, b: &Numeric) -> Result<Numeric, JsonError> {
    Ok(match op {
        ArithOp::Add => a.add(b),
        ArithOp::Sub => a.sub(b),
        ArithOp::Mul => a.mul(b),
        ArithOp::Div => a
            .div(b)
            .map_err(|_| err(DIVISION_BY_ZERO, "division by zero"))?,
        ArithOp::Mod => a
            .modulo(b)
            .map_err(|_| err(DIVISION_BY_ZERO, "division by zero"))?,
    })
}

/// Compare two scalar jsonb items under `op`, three-valued: cross-type or
/// non-scalar operands are Unknown (PG's `jsonb` path comparison semantics).
fn compare_scalars(op: CmpOp, a: &Jsonb, b: &Jsonb) -> Ternary {
    let ord = match (a, b) {
        (Jsonb::Null, Jsonb::Null) => Ordering::Equal,
        (Jsonb::Bool(x), Jsonb::Bool(y)) => x.cmp(y),
        (Jsonb::Number(x), Jsonb::Number(y)) => x.cmp(y),
        (Jsonb::String(x), Jsonb::String(y)) => x.as_bytes().cmp(y.as_bytes()),
        // NULL vs non-NULL: only (in)equality is defined, and it is false/true.
        (Jsonb::Null, _) | (_, Jsonb::Null) => {
            return match op {
                CmpOp::Eq => Ternary::False,
                CmpOp::Ne => Ternary::True,
                _ => Ternary::Unknown,
            };
        }
        // Different scalar types, or an array/object operand: not comparable.
        _ => return Ternary::Unknown,
    };
    Ternary::from_bool(match op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Le => ord != Ordering::Greater,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Ge => ord != Ordering::Less,
    })
}

// ---------------------------------------------------------------------------
// Public evaluation entry points
// ---------------------------------------------------------------------------

/// The name `.type()` reports for an item (`number`, not `numeric`).
fn type_name(v: &Jsonb) -> &'static str {
    match v {
        Jsonb::Null => "null",
        Jsonb::Bool(_) => "boolean",
        Jsonb::Number(_) => "number",
        Jsonb::String(_) => "string",
        Jsonb::Array(_) => "array",
        Jsonb::Object(_) => "object",
    }
}

/// `jsonb_path_query`: the SQL/JSON sequence the path yields against `target`.
/// `silent` suppresses structural errors (returning an empty sequence).
pub fn query(
    p: &JsonPath,
    target: &Jsonb,
    vars: Option<&Jsonb>,
    silent: bool,
) -> Result<Vec<Jsonb>, JsonError> {
    let ev = Eval {
        root: target,
        vars,
        strict: p.strict,
    };
    match ev.seq(&p.expr, target, None) {
        Ok(items) => Ok(items),
        Err(e) if silent && suppressible(&e) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// `jsonb_path_exists` / the `@?` operator: does the path return any item?
/// Returns `None` only when `silent` suppresses an error (SQL NULL).
pub fn exists(
    p: &JsonPath,
    target: &Jsonb,
    vars: Option<&Jsonb>,
    silent: bool,
) -> Result<Option<bool>, JsonError> {
    let ev = Eval {
        root: target,
        vars,
        strict: p.strict,
    };
    match ev.seq(&p.expr, target, None) {
        Ok(items) => Ok(Some(!items.is_empty())),
        Err(e) if silent && suppressible(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `jsonb_path_match` / the `@@` operator: evaluate a predicate path to a single
/// boolean. `Some(bool)` for true/false, `None` for SQL NULL (Unknown, or a
/// suppressed error under `silent`).
pub fn match_predicate(
    p: &JsonPath,
    target: &Jsonb,
    vars: Option<&Jsonb>,
    silent: bool,
) -> Result<Option<bool>, JsonError> {
    let ev = Eval {
        root: target,
        vars,
        strict: p.strict,
    };
    let result = if is_predicate(&p.expr) {
        ev.pred(&p.expr, target)
    } else {
        // A non-predicate path is accepted only if it yields a single boolean.
        match ev.seq(&p.expr, target, None) {
            Ok(items) => match items.as_slice() {
                [Jsonb::Bool(b)] => Ok(Ternary::from_bool(*b)),
                [Jsonb::Null] => Ok(Ternary::Unknown),
                _ => Err(err(
                    SINGLETON_JSON_ITEM_REQUIRED,
                    "single boolean result is expected",
                )),
            },
            Err(e) => Err(e),
        }
    };
    match result {
        Ok(Ternary::True) => Ok(Some(true)),
        Ok(Ternary::False) => Ok(Some(false)),
        Ok(Ternary::Unknown) => Ok(None),
        Err(e) if silent && suppressible(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};

    fn parse(s: &str) -> Result<JsonPath> {
        jsonpath_in(s).map_err(|e| anyhow!("jsonpath_in({s:?}) failed: {}", e.message))
    }

    fn out(s: &str) -> Result<String> {
        Ok(format(&parse(s)?))
    }

    fn jb(s: &str) -> Jsonb {
        crate::json::jsonb_in(s).expect("valid jsonb")
    }

    fn q(target: &str, path: &str) -> Result<Vec<String>> {
        let p = parse(path)?;
        let items = query(&p, &jb(target), None, false)
            .map_err(|e| anyhow!("query failed: {}", e.message))?;
        Ok(items.iter().map(crate::json::format).collect())
    }

    /// PG validates `like_regex` while parsing the path, so these are all
    /// errors on the cast rather than on any row.
    #[test]
    fn like_regex_is_validated_at_parse_time() {
        // An unrecognized flag character, reported with PG's DETAIL.
        let e = jsonpath_in("$ like_regex \"a\" flag \"z\"").expect_err("bad flag");
        assert_eq!(e.sqlstate, "42601");
        assert_eq!(e.message, "invalid input syntax for type jsonpath");
        assert_eq!(
            e.detail.as_deref(),
            Some("Unrecognized flag character \"z\" in LIKE_REGEX predicate.")
        );
        assert_eq!(
            jsonpath_in("$ like_regex \"a\" flag \"hello\"")
                .expect_err("bad flag")
                .detail
                .as_deref(),
            Some("Unrecognized flag character \"h\" in LIKE_REGEX predicate.")
        );

        // `x` is unimplemented, but only rejected when `q` is absent.
        let e = jsonpath_in("$ like_regex \"a\" flag \"ismx\"").expect_err("x needs q");
        assert_eq!(e.sqlstate, "0A000");
        assert_eq!(
            e.message,
            "XQuery \"x\" flag (expanded regular expressions) is not implemented"
        );
        assert!(jsonpath_in("$ like_regex \"a\" flag \"qx\"").is_ok());
        assert!(jsonpath_in("$ like_regex \"a\" flag \"ismxq\"").is_ok());

        // The pattern itself is compiled during the parse. (Only the SQLSTATE
        // is pinned: our message comes from the `regex` crate and differs from
        // PG's wording, as it already does for the `~` operator.)
        assert_eq!(
            jsonpath_in("$ like_regex \"a(\"")
                .expect_err("bad pattern")
                .sqlstate,
            "2201B"
        );
        // Under `q` the pattern is escaped, so a "bad" regex is fine.
        assert!(jsonpath_in("$ like_regex \"a(\" flag \"q\"").is_ok());
    }

    #[test]
    fn like_regex_evaluates_under_flags() -> Result<()> {
        // `q` matches the literal text, including regex metacharacters.
        assert_eq!(
            q("{\"a\":\"a(\"}", "$.a ? (@ like_regex \"a(\" flag \"q\")")?,
            ["\"a(\""]
        );
        // `x` is inert: it survives parsing only with `q`, which already made
        // the pattern literal, so the spaces still have to match.
        assert_eq!(
            q(
                "{\"a\":\"a b\"}",
                "$.a ? (@ like_regex \"a b\" flag \"xq\")"
            )?,
            ["\"a b\""]
        );
        assert!(q("{\"a\":\"ab\"}", "$.a ? (@ like_regex \"a b\" flag \"xq\")")?.is_empty());
        // `i` composes with `q`.
        assert_eq!(
            q(
                "{\"a\":\"A.C\"}",
                "$.a ? (@ like_regex \"a.c\" flag \"qi\")"
            )?,
            ["\"A.C\""]
        );
        // `s` lets `.` span a newline; without it, it must not.
        assert_eq!(
            q(
                "{\"a\":\"a\\nb\"}",
                "$.a ? (@ like_regex \"a.b\" flag \"s\")"
            )?
            .len(),
            1
        );
        assert!(q("{\"a\":\"a\\nb\"}", "$.a ? (@ like_regex \"a.b\")")?.is_empty());

        Ok(())
    }

    #[test]
    fn output_canonical_form() -> Result<()> {
        assert_eq!(out("$.a.b[*] ? (@ > 3)")?, "$.\"a\".\"b\"[*]?(@ > 3)");
        assert_eq!(
            out("lax $.\"a b\"[1 to 3, 5].size()")?,
            "$.\"a b\"[1 to 3,5].size()"
        );
        assert_eq!(
            out("strict $.a.**{2 to 4}.c")?,
            "strict $.\"a\".**{2 to 4}.\"c\""
        );
        assert_eq!(
            out("$.a + $.b * 2 - (-3)")?,
            "(($.\"a\" + $.\"b\" * 2) - -3)"
        );
        assert_eq!(
            out("$ ? (@ like_regex \"ab.*c\" flag \"i\")")?,
            "$?(@ like_regex \"ab.*c\" flag \"i\")"
        );
        // Flags are re-emitted from the parsed set: fixed order, deduplicated,
        // and omitted entirely when empty.
        assert_eq!(
            out("$ ? (@ like_regex \"a\" flag \"qmi\")")?,
            "$?(@ like_regex \"a\" flag \"imq\")"
        );
        assert_eq!(
            out("$ ? (@ like_regex \"a\" flag \"ii\")")?,
            "$?(@ like_regex \"a\" flag \"i\")"
        );
        assert_eq!(
            out("$ ? (@ like_regex \"a\" flag \"\")")?,
            "$?(@ like_regex \"a\")"
        );
        assert_eq!(
            out("$ ? (@ like_regex \"a\" flag \"xq\")")?,
            "$?(@ like_regex \"a\" flag \"xq\")"
        );
        // Canonical output must re-parse: `x` is emitted before the `q` that
        // makes it legal.
        assert_eq!(
            out(&out("$ ? (@ like_regex \"a\" flag \"qx\")")?)?,
            "$?(@ like_regex \"a\" flag \"xq\")"
        );
        assert_eq!(
            out("$ ? (@.name starts with \"Jo\")")?,
            "$?(@.\"name\" starts with \"Jo\")"
        );
        assert_eq!(out("$ ? (exists (@.x))")?, "$?(exists (@.\"x\"))");
        assert_eq!(out("$ ? ((@ > 1) is unknown)")?, "$?((@ > 1) is unknown)");
        assert_eq!(out("$[last]")?, "$[last]");
        assert_eq!(out("$size")?, "$\"size\"");
        assert_eq!(out("1e3")?, "1000");
        Ok(())
    }

    #[test]
    fn navigation_and_filters() -> Result<()> {
        assert_eq!(q("{\"a\":[1,2,3]}", "$.a[*] ? (@ > 1)")?, vec!["2", "3"]);
        assert_eq!(q("{\"a\":[1,2,3]}", "$.a ? (@ >= 2)")?, vec!["2", "3"]);
        assert_eq!(
            q("[{\"x\":1},{\"x\":9}]", "$ ? (@.x > 5)")?,
            vec!["{\"x\": 9}"]
        );
        assert_eq!(
            q("{\"a\":{\"b\":1,\"c\":{\"b\":2}}}", "$.**.b")?,
            vec!["1", "2"]
        );
        Ok(())
    }

    #[test]
    fn methods() -> Result<()> {
        assert_eq!(
            q("[1,\"x\",true,null,{},[2]]", "$[*].type()")?,
            vec![
                "\"number\"",
                "\"string\"",
                "\"boolean\"",
                "\"null\"",
                "\"object\"",
                "\"array\""
            ]
        );
        assert_eq!(q("5", "$.size()")?, vec!["1"]);
        assert_eq!(q("[1,2,3]", "$.size()")?, vec!["3"]);
        assert_eq!(q("\"1.5\"", "$.double()")?, vec!["1.5"]);
        assert_eq!(q("-2.3", "$.abs()")?, vec!["2.3"]);
        assert_eq!(q("2.3", "$.floor()")?, vec!["2"]);
        assert_eq!(q("2.3", "$.ceiling()")?, vec!["3"]);
        assert_eq!(
            q("{\"a\":1,\"b\":2}", "$.keyvalue()")?,
            vec![
                "{\"id\": 0, \"key\": \"a\", \"value\": 1}",
                "{\"id\": 0, \"key\": \"b\", \"value\": 2}"
            ]
        );
        Ok(())
    }

    #[test]
    fn arithmetic() -> Result<()> {
        assert_eq!(q("{\"a\":5,\"b\":2}", "$.a + $.b")?, vec!["7"]);
        assert_eq!(q("{\"a\":5}", "$.a * 2 + 1")?, vec!["11"]);
        assert_eq!(q("{}", "(1.50 + 2.5)")?, vec!["4.00"]);
        assert_eq!(q("{}", "(7 % 3)")?, vec!["1"]);
        Ok(())
    }

    /// The error text/message returned by evaluating `path` against `target`.
    fn qerr(target: &str, path: &str) -> JsonError {
        query(&parse(path).expect("parse"), &jb(target), None, false).unwrap_err()
    }

    #[test]
    fn lax_methods_unwrap_arrays_one_level() -> Result<()> {
        // lax auto-unwraps a bare array for the numeric / keyvalue methods.
        assert_eq!(q("[1,-2,3]", "$.abs()")?, vec!["1", "2", "3"]);
        assert_eq!(q("[1.5,2.5]", "$.floor()")?, vec!["1", "2"]);
        assert_eq!(q("[\"1.5\",2]", "$.double()")?, vec!["1.5", "2"]);
        // ...one level only: an element that is itself an array errors.
        assert_eq!(
            qerr("[[1],[-2]]", "$.abs()").message,
            "jsonpath item method .abs() can only be applied to a numeric value"
        );
        // `.size()`/`.type()` never unwrap (they describe the container).
        assert_eq!(q("[1,2,3]", "$.size()")?, vec!["3"]);
        assert_eq!(q("[1,2]", "$.type()")?, vec!["\"array\""]);
        Ok(())
    }

    #[test]
    fn strict_size_requires_array() {
        // strict `.size()` on a non-array errors; lax returns 1.
        let e = qerr("5", "strict $.size()");
        assert_eq!(e.sqlstate, SQL_JSON_ARRAY_NOT_FOUND);
        assert_eq!(
            e.message,
            "jsonpath item method .size() can only be applied to an array"
        );
    }

    #[test]
    fn arith_errors_name_the_operator() {
        // The "single numeric value" error names the actual operator (22038).
        let e = qerr("[1,2]", "$[*] * 1");
        assert_eq!(e.sqlstate, SINGLETON_JSON_ITEM_REQUIRED);
        assert_eq!(
            e.message,
            "left operand of jsonpath operator * is not a single numeric value"
        );
        assert_eq!(
            qerr("[1,2]", "1 - $[*]").message,
            "right operand of jsonpath operator - is not a single numeric value"
        );
        // Unary has its own message/SQLSTATE (2203B).
        let u = qerr("{\"a\":\"x\"}", "- $.a");
        assert_eq!(u.sqlstate, SQL_JSON_NUMBER_NOT_FOUND);
        assert_eq!(
            u.message,
            "operand of unary jsonpath operator - is not a numeric value"
        );
    }

    #[test]
    fn predicates_and_matching() -> Result<()> {
        assert_eq!(
            match_predicate(&parse("$.a == 1")?, &jb("{\"a\":1}"), None, false).unwrap(),
            Some(true)
        );
        assert_eq!(
            match_predicate(&parse("$.a == 2")?, &jb("{\"a\":1}"), None, false).unwrap(),
            Some(false)
        );
        // Type-mismatch comparison → Unknown → SQL NULL.
        assert_eq!(
            match_predicate(&parse("$.a > 1")?, &jb("{\"a\":\"x\"}"), None, false).unwrap(),
            None
        );
        // Predicate query yields one boolean item.
        assert_eq!(q("{\"a\":1}", "$.a > 0")?, vec!["true"]);
        assert_eq!(q("{\"a\":\"x\"}", "$.a > 1")?, vec!["null"]);
        assert_eq!(
            q("{\"a\":\"hello\"}", "$.a ? (@ starts with \"he\")")?,
            vec!["\"hello\""]
        );
        assert_eq!(
            q("{\"a\":\"hello\"}", "$.a ? (@ like_regex \"^h.*o$\")")?,
            vec!["\"hello\""]
        );
        Ok(())
    }

    #[test]
    fn strict_vs_lax_and_silent() -> Result<()> {
        // lax member on scalar → empty.
        assert!(q("1", "$.a")?.is_empty());
        // strict member-not-found → error.
        let e = query(&parse("strict $.b")?, &jb("{\"a\":1}"), None, false).unwrap_err();
        assert_eq!(e.message, "JSON object does not contain key \"b\"");
        // silent suppresses it.
        assert!(
            query(&parse("strict $.b")?, &jb("{\"a\":1}"), None, true)
                .unwrap()
                .is_empty()
        );
        // out-of-range subscript: strict errors, lax skips.
        assert_eq!(
            query(&parse("strict $[5]")?, &jb("[1,2]"), None, false)
                .unwrap_err()
                .message,
            "jsonpath array subscript is out of bounds"
        );
        assert!(q("[1,2]", "$[5]")?.is_empty());
        Ok(())
    }

    #[test]
    fn variables() -> Result<()> {
        let vars = jb("{\"min\":3}");
        assert_eq!(q_vars("{\"x\":5}", "$.x ? (@ >= $min)", &vars)?, vec!["5"]);
        // Missing variable is a hard error even under silent.
        let e = query(&parse("$.x ? (@ >= $min)")?, &jb("{\"x\":5}"), None, true).unwrap_err();
        assert_eq!(e.sqlstate, UNDEFINED_OBJECT);
        assert_eq!(e.message, "could not find jsonpath variable \"min\"");
        Ok(())
    }

    fn q_vars(target: &str, path: &str, vars: &Jsonb) -> Result<Vec<String>> {
        let p = parse(path)?;
        let items = query(&p, &jb(target), Some(vars), false)
            .map_err(|e| anyhow!("query failed: {}", e.message))?;
        Ok(items.iter().map(crate::json::format).collect())
    }

    #[test]
    fn parse_errors() {
        assert_eq!(
            jsonpath_in("$.").unwrap_err().message,
            "syntax error at end of jsonpath input"
        );
        assert_eq!(jsonpath_in("foo").unwrap_err().sqlstate, SYNTAX_ERROR);
        assert!(
            jsonpath_in("5.double()")
                .unwrap_err()
                .message
                .contains("trailing junk after numeric literal")
        );
        // `.**` requires the dot.
        assert!(jsonpath_in("$**.a").is_err());
    }
}
