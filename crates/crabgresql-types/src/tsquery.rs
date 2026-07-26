//! `tsquery`: a boolean search expression over lexemes, and the `@@` match
//! operator that evaluates it against a [`TsVector`].
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the accepted input spellings, the canonical `tsquery_out` text
//! (including its exact parenthesization and spacing), the match semantics, and
//! the SQLSTATE/message of each error — derived from the documentation and from
//! differential probing against a real server, and implemented independently.
//!
//! Representation: a tree ([`Node`]). The tree shape is significant — `'1|2|4'` and `'1|(2|4)'` are distinct
//! values in PG even though they print identically — so parsing never
//! re-associates.

use crate::tsvector::{self, TsError, TsVector};
use std::cmp::Ordering;

const SYNTAX_ERROR: &str = "42601";
const INVALID_PARAMETER_VALUE: &str = "22023";
const PROGRAM_LIMIT_EXCEEDED: &str = "54001";

/// Largest accepted `<N>` phrase distance.
pub const MAX_DISTANCE: u32 = 16384;

/// Maximum *parser* recursion, counting `(` and `!` — the only constructs that
/// recurse in the recursive-descent parser, at five frames per level. `&`/`|`/
/// `<->` chains are parsed by loops and cost no stack, so they are bounded by
/// [`MAX_NODE_DEPTH`] instead.
const MAX_PARSE_DEPTH: usize = 200;

/// Maximum nesting of the resulting [`Node`] tree. Every later walk over a
/// query — [`format`], [`cmp`], [`numnode`], matching, and `Node`'s recursive
/// `Drop` — recurses once per level, so the tree has to be bounded even when
/// building it did not recurse: `'a&a&a&…'` is parsed by a loop but produces a
/// left spine as deep as the term count.
///
/// Measured on a 2 MiB worker-thread stack (debug, the larger-frame case): the
/// cheap walks survive a depth of 2000, but [`decode`] — the deepest frame, and
/// the one reached when a stored query is read back — survives 800 and
/// overflows by 1000. The cap is set below that worst walk with ~2x margin, so
/// nothing the parser accepts can crash on its way back off the heap. PG's own
/// limit is far higher; it reports `tsquery stack too small` when its parser
/// stack fills.
const MAX_NODE_DEPTH: usize = 400;

/// Weight-filter bits. A query operand may restrict which weights count as a
/// match; an empty mask means "any weight".
pub const W_A: u8 = 1 << 3;
pub const W_B: u8 = 1 << 2;
pub const W_C: u8 = 1 << 1;
pub const W_D: u8 = 1 << 0;

/// One node of the query tree.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Node {
    /// A lexeme, optionally a prefix match (`:*`) and/or weight-restricted
    /// (`:AB`). `weights == 0` means any weight.
    Val {
        word: String,
        prefix: bool,
        weights: u8,
    },
    Not(Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    /// `left <dist> right`. `<->` is `dist == 1`.
    Phrase {
        dist: u16,
        left: Box<Node>,
        right: Box<Node>,
    },
}

/// A `tsquery`. An empty query (`''::tsquery`) has no root and matches nothing.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct TsQuery {
    pub root: Option<Node>,
}

/// PG's wording when its parser stack fills; ours reports the same thing when a
/// query nests deeper than we can safely walk.
fn too_deep() -> TsError {
    TsError::new(PROGRAM_LIMIT_EXCEEDED, "tsquery stack too small")
}

fn syntax(input: &str) -> TsError {
    TsError::new(
        SYNTAX_ERROR,
        format!("syntax error in tsquery: \"{input}\""),
    )
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// `tsquery_in`. Returns the parsed query; an input with no lexemes yields an
/// empty query, which the caller reports with PG's
/// `text-search query doesn't contain lexemes` NOTICE.
pub fn tsquery_in(input: &str) -> Result<TsQuery, TsError> {
    let mut p = Parser {
        chars: input.chars().collect(),
        pos: 0,
        input,
        depth: 0,
    };
    p.skip_ws();
    if p.pos >= p.chars.len() {
        return Ok(TsQuery::default());
    }
    let (root, _) = p.parse_or()?;
    p.skip_ws();
    if p.pos < p.chars.len() {
        return Err(syntax(input));
    }
    Ok(TsQuery { root: Some(root) })
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    input: &'a str,
    /// Current parser recursion depth, bounded by [`MAX_PARSE_DEPTH`].
    depth: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
            self.pos += 1;
        }
    }

    /// Run `f` one parser level deeper, refusing to recurse past
    /// [`MAX_PARSE_DEPTH`] so a pathological literal cannot overflow the stack.
    fn descend(
        &mut self,
        f: fn(&mut Self) -> Result<(Node, usize), TsError>,
    ) -> Result<(Node, usize), TsError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(too_deep());
        }
        let node = f(self)?;
        self.depth -= 1;
        Ok(node)
    }

    /// The depth of a node combining two subtrees, refusing to build one deeper
    /// than [`MAX_NODE_DEPTH`].
    fn joined(a: usize, b: usize) -> Result<usize, TsError> {
        let d = a.max(b) + 1;
        if d > MAX_NODE_DEPTH {
            return Err(too_deep());
        }
        Ok(d)
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_ws();
        self.chars.get(self.pos).copied()
    }

    /// `orexpr := andexpr ( '|' andexpr )*`, left-associative. Returns the
    /// subtree and its depth.
    fn parse_or(&mut self) -> Result<(Node, usize), TsError> {
        let (mut left, mut depth) = self.parse_and()?;
        while self.peek() == Some('|') {
            self.pos += 1;
            let (right, rd) = self.parse_and()?;
            depth = Self::joined(depth, rd)?;
            left = Node::Or(Box::new(left), Box::new(right));
        }
        Ok((left, depth))
    }

    /// `andexpr := phrexpr ( '&' phrexpr )*`, left-associative.
    fn parse_and(&mut self) -> Result<(Node, usize), TsError> {
        let (mut left, mut depth) = self.parse_phrase()?;
        while self.peek() == Some('&') {
            self.pos += 1;
            let (right, rd) = self.parse_phrase()?;
            depth = Self::joined(depth, rd)?;
            left = Node::And(Box::new(left), Box::new(right));
        }
        Ok((left, depth))
    }

    /// `phrexpr := notexpr ( '<->' | '<N>' notexpr )*`, left-associative.
    fn parse_phrase(&mut self) -> Result<(Node, usize), TsError> {
        let (mut left, mut depth) = self.parse_not()?;
        while self.peek() == Some('<') {
            let dist = self.scan_distance()?;
            // A phrase operator with nothing after it is a plain syntax error,
            // unlike `&`/`|`/`!`, which report "no operand".
            if self.peek().is_none() {
                return Err(syntax(self.input));
            }
            let (right, rd) = self.parse_not()?;
            depth = Self::joined(depth, rd)?;
            left = Node::Phrase {
                dist,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        Ok((left, depth))
    }

    /// Consume `<->` or `<N>`, returning the distance.
    fn scan_distance(&mut self) -> Result<u16, TsError> {
        // `self.peek()` already skipped whitespace and saw '<'.
        self.pos += 1;
        if self.chars.get(self.pos) == Some(&'-') && self.chars.get(self.pos + 1) == Some(&'>') {
            self.pos += 2;
            return Ok(1);
        }
        let start = self.pos;
        let mut n: u32 = 0;
        while let Some(d) = self.chars.get(self.pos).and_then(|c| c.to_digit(10)) {
            n = n.saturating_mul(10).saturating_add(d);
            self.pos += 1;
        }
        if self.pos == start || self.chars.get(self.pos) != Some(&'>') {
            return Err(syntax(self.input));
        }
        self.pos += 1;
        if n > MAX_DISTANCE {
            return Err(TsError::new(
                INVALID_PARAMETER_VALUE,
                "distance in phrase operator must be an integer value between zero and 16384 inclusive",
            ));
        }
        Ok(n as u16)
    }

    /// `notexpr := '!' notexpr | primary`.
    fn parse_not(&mut self) -> Result<(Node, usize), TsError> {
        if self.peek() == Some('!') {
            self.pos += 1;
            let (inner, d) = self.descend(Self::parse_not)?;
            return Ok((Node::Not(Box::new(inner)), Self::joined(d, 0)?));
        }
        self.parse_primary()
    }

    /// `primary := '(' orexpr ')' | value`.
    fn parse_primary(&mut self) -> Result<(Node, usize), TsError> {
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let inner = self.descend(Self::parse_or)?;
                if self.peek() != Some(')') {
                    return Err(syntax(self.input));
                }
                self.pos += 1;
                Ok(inner)
            }
            // An operator where an operand belongs — PG distinguishes a trailing
            // operator ("no operand") from other syntax errors.
            Some('&') | Some('|') | Some(')') | Some('<') => Err(syntax(self.input)),
            None => Err(TsError::new(
                SYNTAX_ERROR,
                format!("no operand in tsquery: \"{}\"", self.input),
            )),
            _ => self.parse_value(),
        }
    }

    /// A lexeme with its optional `:` weight/prefix suffix.
    fn parse_value(&mut self) -> Result<(Node, usize), TsError> {
        let word = self.scan_lexeme()?;
        if word.is_empty() {
            return Err(syntax(self.input));
        }
        let mut prefix = false;
        let mut weights = 0u8;
        if self.chars.get(self.pos) == Some(&':') {
            self.pos += 1;
            // Weight letters and `*` may appear in any order and repeat;
            // `'doo:a*'` and `'doo:*a'` are the same query.
            loop {
                let bit = match self.chars.get(self.pos) {
                    Some('*') => {
                        prefix = true;
                        self.pos += 1;
                        continue;
                    }
                    Some('A' | 'a') => W_A,
                    Some('B' | 'b') => W_B,
                    Some('C' | 'c') => W_C,
                    Some('D' | 'd') => W_D,
                    _ => break,
                };
                weights |= bit;
                self.pos += 1;
            }
        }
        // Whatever follows must be an operator, a paren, whitespace or the end —
        // two adjacent lexemes (`'a b'`) are a syntax error, not an implicit AND.
        match self.chars.get(self.pos) {
            None => {}
            Some(c) if c.is_whitespace() || "&|<)".contains(*c) => {}
            Some(_) => return Err(syntax(self.input)),
        }
        Ok((
            Node::Val {
                word,
                prefix,
                weights,
            },
            1,
        ))
    }

    /// Read one lexeme. The spelling is identical to `tsvector`'s, so the
    /// scanner is shared; only the characters that end a *bare* lexeme differ,
    /// since an operator may follow one without whitespace (`a&b`).
    fn scan_lexeme(&mut self) -> Result<String, TsError> {
        const STOPS: &[char] = &[':', '&', '|', '!', '(', ')', '<'];
        tsvector::scan_lexeme(&self.chars, &mut self.pos, STOPS, || syntax(self.input))
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Operator binding strength, used to decide parenthesization. Higher binds
/// tighter.
fn prec(node: &Node) -> u8 {
    match node {
        Node::Or(..) => 1,
        Node::And(..) => 2,
        Node::Phrase { .. } => 3,
        Node::Not(_) => 4,
        Node::Val { .. } => 5,
    }
}

/// `tsquery_out`: the canonical text form.
///
/// Parenthesization matches PG exactly: a child is wrapped when it binds more
/// loosely than its parent, plus the one special case that a phrase's *right*
/// operand is wrapped when it is itself a phrase — so `'a<->b<->c'` prints flat
/// but `'a<->(b<->c)'` keeps its parentheses. Note this rendering is lossy for
/// `&`/`|`: `'1|2|4'` and `'1|(2|4)'` are distinct values that print alike, as
/// they do in PG.
pub fn format(q: &TsQuery) -> String {
    let mut out = String::new();
    if let Some(root) = &q.root {
        format_node(root, &mut out);
    }
    out
}

fn format_node(node: &Node, out: &mut String) {
    match node {
        Node::Val {
            word,
            prefix,
            weights,
        } => {
            tsvector::format_lexeme(word, out);
            if *prefix || *weights != 0 {
                out.push(':');
                if *prefix {
                    out.push('*');
                }
                for (bit, ch) in [(W_A, 'A'), (W_B, 'B'), (W_C, 'C'), (W_D, 'D')] {
                    if weights & bit != 0 {
                        out.push(ch);
                    }
                }
            }
        }
        Node::Not(inner) => {
            out.push('!');
            format_child(node, inner, false, out);
        }
        Node::And(l, r) => {
            format_child(node, l, false, out);
            out.push_str(" & ");
            format_child(node, r, true, out);
        }
        Node::Or(l, r) => {
            format_child(node, l, false, out);
            out.push_str(" | ");
            format_child(node, r, true, out);
        }
        Node::Phrase { dist, left, right } => {
            format_child(node, left, false, out);
            if *dist == 1 {
                out.push_str(" <-> ");
            } else {
                out.push_str(&format!(" <{dist}> "));
            }
            format_child(node, right, true, out);
        }
    }
}

fn format_child(parent: &Node, child: &Node, is_right: bool, out: &mut String) {
    let same_phrase =
        is_right && matches!(parent, Node::Phrase { .. }) && matches!(child, Node::Phrase { .. });
    if prec(child) < prec(parent) || same_phrase {
        out.push_str("( ");
        format_node(child, out);
        out.push_str(" )");
    } else {
        format_node(child, out);
    }
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// `numnode(tsquery)`: the number of nodes, operators included.
pub fn numnode(q: &TsQuery) -> i32 {
    fn count(n: &Node) -> i32 {
        match n {
            Node::Val { .. } => 1,
            Node::Not(x) => 1 + count(x),
            Node::And(l, r) | Node::Or(l, r) => 1 + count(l) + count(r),
            Node::Phrase { left, right, .. } => 1 + count(left) + count(right),
        }
    }
    q.root.as_ref().map_or(0, count)
}

/// Total bytes of all lexemes in the query — the second tier of the sort order.
fn lexeme_bytes(q: &TsQuery) -> usize {
    fn walk(n: &Node) -> usize {
        match n {
            Node::Val { word, .. } => word.len(),
            Node::Not(x) => walk(x),
            Node::And(l, r) | Node::Or(l, r) => walk(l) + walk(r),
            Node::Phrase { left, right, .. } => walk(left) + walk(right),
        }
    }
    q.root.as_ref().map_or(0, walk)
}

/// Operator rank within the third tier: phrase nodes sort before `|`, which
/// sorts before `&`, and phrases order among themselves by distance.
fn op_rank(n: &Node) -> (u8, u16) {
    match n {
        Node::Val { .. } => (0, 0),
        Node::Not(_) => (1, 0),
        Node::Phrase { dist, .. } => (2, *dist),
        Node::Or(..) => (3, 0),
        Node::And(..) => (4, 0),
    }
}

/// PG's `tsquery` total order: node count, then total lexeme bytes, then a
/// structural walk comparing the operator first and the **right** child before
/// the left.
///
/// Like PG, the leaf tier looks only at the lexeme: `'a' = 'a:*'` and
/// `'a:A' = 'a:B'` are true, so a prefix flag or weight mask never separates two
/// otherwise equal queries.
///
/// The one divergence is the *order* of two distinct lexemes, which comes out in an order with no
/// relation to the text (`'d' < 'm' < 'a' < 'e' < …` on 18.4). That order has no
/// documented contract, so we use byte order; every comparison the upstream
/// `tstypes` suite asserts is decided by one of the tiers above it.
pub fn cmp(a: &TsQuery, b: &TsQuery) -> Ordering {
    numnode(a)
        .cmp(&numnode(b))
        .then_with(|| lexeme_bytes(a).cmp(&lexeme_bytes(b)))
        .then_with(|| match (&a.root, &b.root) {
            (Some(x), Some(y)) => cmp_node(x, y),
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
        })
}

fn cmp_node(a: &Node, b: &Node) -> Ordering {
    let ord = op_rank(a).cmp(&op_rank(b));
    if ord != Ordering::Equal {
        return ord;
    }
    match (a, b) {
        (
            Node::Val {
                word: w1,
                prefix: p1,
                weights: g1,
            },
            Node::Val {
                word: w2,
                prefix: p2,
                weights: g2,
            },
        ) => {
            // PG compares only the lexeme here: `'a' = 'a:*'` and
            // `'a:A' = 'a:B'` are both true, and `SELECT DISTINCT` collapses
            // them. Including the prefix flag or the weight mask would make
            // equality stricter than PG's, since `=`/`DISTINCT`/`GROUP BY` all
            // route through this comparison.
            let _ = (p1, p2, g1, g2);
            w1.as_bytes().cmp(w2.as_bytes())
        }
        (Node::Not(x), Node::Not(y)) => cmp_node(x, y),
        (Node::And(l1, r1), Node::And(l2, r2))
        | (Node::Or(l1, r1), Node::Or(l2, r2))
        | (
            Node::Phrase {
                left: l1,
                right: r1,
                ..
            },
            Node::Phrase {
                left: l2,
                right: r2,
                ..
            },
        ) => cmp_node(r1, r2).then_with(|| cmp_node(l1, l2)),
        // `op_rank` already separated every other pairing.
        _ => Ordering::Equal,
    }
}

// ---------------------------------------------------------------------------
// Combinators
// ---------------------------------------------------------------------------

/// The nesting depth of a node. Safe to recurse: every [`TsQuery`] in existence
/// is built by the parser or by the combinators below, both of which refuse to
/// exceed [`MAX_NODE_DEPTH`].
fn depth(n: &Node) -> usize {
    1 + match n {
        Node::Val { .. } => 0,
        Node::Not(x) => depth(x),
        Node::And(l, r) | Node::Or(l, r) => depth(l).max(depth(r)),
        Node::Phrase { left, right, .. } => depth(left).max(depth(right)),
    }
}

/// Combine two queries, dropping an empty operand (PG's `&&`/`||`/`<->`
/// operators treat an empty query as absent). Errors rather than building a tree
/// too deep to walk — repeated `q = q && 'x'` would otherwise grow one a level
/// at a time, past anything the parser would have accepted in one literal.
fn combine(
    a: &TsQuery,
    b: &TsQuery,
    make: impl FnOnce(Node, Node) -> Node,
) -> Result<TsQuery, TsError> {
    match (&a.root, &b.root) {
        (Some(x), Some(y)) => {
            if depth(x).max(depth(y)) + 1 > MAX_NODE_DEPTH {
                return Err(too_deep());
            }
            Ok(TsQuery {
                root: Some(make(x.clone(), y.clone())),
            })
        }
        (Some(_), None) => Ok(a.clone()),
        (None, Some(_)) => Ok(b.clone()),
        (None, None) => Ok(TsQuery::default()),
    }
}

/// `tsquery && tsquery`.
pub fn and(a: &TsQuery, b: &TsQuery) -> Result<TsQuery, TsError> {
    combine(a, b, |x, y| Node::And(Box::new(x), Box::new(y)))
}

/// `tsquery || tsquery`.
pub fn or(a: &TsQuery, b: &TsQuery) -> Result<TsQuery, TsError> {
    combine(a, b, |x, y| Node::Or(Box::new(x), Box::new(y)))
}

/// `!! tsquery`.
pub fn not(a: &TsQuery) -> Result<TsQuery, TsError> {
    if a.root
        .as_ref()
        .is_some_and(|n| depth(n) + 1 > MAX_NODE_DEPTH)
    {
        return Err(too_deep());
    }
    Ok(TsQuery {
        root: a.root.clone().map(|n| Node::Not(Box::new(n))),
    })
}

/// `tsquery <-> tsquery` and `tsquery_phrase(a, b, distance)`.
pub fn phrase(a: &TsQuery, b: &TsQuery, dist: u16) -> Result<TsQuery, TsError> {
    combine(a, b, |x, y| Node::Phrase {
        dist,
        left: Box::new(x),
        right: Box::new(y),
    })
}

/// `querytree(tsquery)`: the indexable part of the query, with negated branches
/// removed. Prints `T` when nothing indexable remains — `!foo` constrains
/// nothing, so an index cannot narrow the scan.
pub fn querytree(q: &TsQuery) -> String {
    /// `None` means "this branch imposes no constraint".
    fn clean(n: &Node) -> Option<Node> {
        match n {
            Node::Val { .. } => Some(n.clone()),
            // A negated branch is not indexable at all.
            Node::Not(_) => None,
            Node::And(l, r) => match (clean(l), clean(r)) {
                (Some(x), Some(y)) => Some(Node::And(Box::new(x), Box::new(y))),
                // AND with an unconstrained side keeps the constrained side.
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            },
            // OR with an unconstrained side is itself unconstrained.
            Node::Or(l, r) => match (clean(l), clean(r)) {
                (Some(x), Some(y)) => Some(Node::Or(Box::new(x), Box::new(y))),
                _ => None,
            },
            // A phrase still requires both lexemes to be present, so it narrows
            // like an AND once positions are ignored.
            Node::Phrase { dist, left, right } => match (clean(left), clean(right)) {
                (Some(x), Some(y)) => Some(Node::Phrase {
                    dist: *dist,
                    left: Box::new(x),
                    right: Box::new(y),
                }),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            },
        }
    }

    // An empty query has nothing to strip, so it prints as nothing; `T` is
    // reserved for a query whose every branch was negated away.
    let Some(root) = q.root.as_ref() else {
        return String::new();
    };
    match clean(root) {
        Some(n) => format(&TsQuery { root: Some(n) }),
        None => "T".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

// Node tags for [`encode`]/[`decode`]. Never reordered — they are part of the
// on-disk datum format.
const T_VAL: u8 = 0;
const T_NOT: u8 = 1;
const T_AND: u8 = 2;
const T_OR: u8 = 3;
const T_PHRASE: u8 = 4;

/// Serialize a query for storage, in prefix order.
///
/// The canonical text form cannot be used here: [`format`] is deliberately lossy
/// for `&`/`|` associativity (`'1|2|4'` and `'1|(2|4)'` print alike but are
/// distinct values), so storing text and re-parsing would silently rewrite one
/// into the other. This keeps the tree shape.
pub fn encode(q: &TsQuery) -> Vec<u8> {
    fn put(n: &Node, out: &mut Vec<u8>) {
        match n {
            Node::Val {
                word,
                prefix,
                weights,
            } => {
                out.push(T_VAL);
                out.push(u8::from(*prefix));
                out.push(*weights);
                out.extend_from_slice(&(word.len() as u32).to_le_bytes());
                out.extend_from_slice(word.as_bytes());
            }
            Node::Not(x) => {
                out.push(T_NOT);
                put(x, out);
            }
            Node::And(l, r) => {
                out.push(T_AND);
                put(l, out);
                put(r, out);
            }
            Node::Or(l, r) => {
                out.push(T_OR);
                put(l, out);
                put(r, out);
            }
            Node::Phrase { dist, left, right } => {
                out.push(T_PHRASE);
                out.extend_from_slice(&dist.to_le_bytes());
                put(left, out);
                put(right, out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(root) = &q.root {
        put(root, &mut out);
    }
    out
}

/// Inverse of [`encode`]. `None` if the bytes are malformed or nest deeper than
/// [`MAX_NODE_DEPTH`] — both impossible for a datum this build wrote, but
/// checked so a corrupt page cannot overflow the stack.
pub fn decode(bytes: &[u8]) -> Option<TsQuery> {
    fn get(b: &[u8], i: &mut usize, depth: usize) -> Option<Node> {
        if depth > MAX_NODE_DEPTH {
            return None;
        }
        let tag = *b.get(*i)?;
        *i += 1;
        Some(match tag {
            T_VAL => {
                let prefix = *b.get(*i)? != 0;
                let weights = *b.get(*i + 1)?;
                *i += 2;
                let len = u32::from_le_bytes(b.get(*i..*i + 4)?.try_into().ok()?) as usize;
                *i += 4;
                let word = std::str::from_utf8(b.get(*i..*i + len)?).ok()?.to_string();
                *i += len;
                Node::Val {
                    word,
                    prefix,
                    weights,
                }
            }
            T_NOT => Node::Not(Box::new(get(b, i, depth + 1)?)),
            T_AND => {
                let l = get(b, i, depth + 1)?;
                Node::And(Box::new(l), Box::new(get(b, i, depth + 1)?))
            }
            T_OR => {
                let l = get(b, i, depth + 1)?;
                Node::Or(Box::new(l), Box::new(get(b, i, depth + 1)?))
            }
            T_PHRASE => {
                let dist = u16::from_le_bytes(b.get(*i..*i + 2)?.try_into().ok()?);
                *i += 2;
                let left = get(b, i, depth + 1)?;
                Node::Phrase {
                    dist,
                    left: Box::new(left),
                    right: Box::new(get(b, i, depth + 1)?),
                }
            }
            _ => return None,
        })
    }
    if bytes.is_empty() {
        return Some(TsQuery::default());
    }
    let mut i = 0;
    let root = get(bytes, &mut i, 1)?;
    (i == bytes.len()).then_some(TsQuery { root: Some(root) })
}

// ---------------------------------------------------------------------------
// Matching (`@@`)
// ---------------------------------------------------------------------------

/// The lexemes a `Val` node's word selects. `TsVector.lexemes` is sorted by byte
/// order, so both an exact match and a prefix match are a *contiguous* run: seek
/// the lower bound once instead of scanning the whole document per node.
fn matching_lexemes<'a>(tv: &'a TsVector, needle: &str, prefix: bool) -> &'a [tsvector::Lexeme] {
    let lower = tv
        .lexemes
        .partition_point(|l| l.word.as_bytes() < needle.as_bytes());
    let rest = &tv.lexemes[lower..];
    let len = if prefix {
        rest.iter()
            .take_while(|l| l.word.as_bytes().starts_with(needle.as_bytes()))
            .count()
    } else {
        usize::from(rest.first().is_some_and(|l| l.word == needle))
    };
    &rest[..len]
}

/// Whether a position passes the node's weight filter. An empty mask means
/// "any weight".
fn weight_ok(weights: u8, w: u8) -> bool {
    weights == 0 || weights & (1 << w) != 0
}

/// Every position at which a `Val` node matches, honoring prefix and weight
/// filters. Sorted and deduplicated, which every `PosSet` consumer relies on.
fn lexeme_positions(tv: &TsVector, node: &Node) -> Vec<u16> {
    let Node::Val {
        word,
        prefix,
        weights,
    } = node
    else {
        return Vec::new();
    };
    let mut out: Vec<u16> = matching_lexemes(tv, word, *prefix)
        .iter()
        .flat_map(|l| l.positions.iter())
        .filter(|p| weight_ok(*weights, p.weight))
        .map(|p| p.pos)
        .collect();
    // One lexeme's positions are already ordered, but a prefix match unions
    // several runs, so normalize.
    out.sort_unstable();
    out.dedup();
    out
}

/// One phrase match, spanning positions `start ..= end`. A bare lexeme has
/// `width == 0`; combining `a <N> b` yields a match reaching from `a`'s start to
/// `b`'s end.
///
/// Positions are signed because a negation matches *outside* the document too:
/// `'a:1' @@ '!c <2> a'` is true, with `!c` sitting at position -1. Clamping at
/// zero silently loses that whole family of matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PMatch {
    end: i32,
    width: i32,
}

impl PMatch {
    fn at(end: i32) -> PMatch {
        PMatch { end, width: 0 }
    }

    fn start(self) -> i32 {
        self.end - self.width
    }

    /// The span reaching from `self`'s start to `to`'s end.
    fn spanning(self, to: PMatch) -> PMatch {
        PMatch {
            end: to.end,
            width: to.end - self.start(),
        }
    }
}

/// Where a subexpression matches, inside a phrase operand.
///
/// Three states, not two.
///
/// A negation yields a *cofinite* set — every position except the ones the
/// negated term occupies — kept symbolically rather than materialized. It also
/// has to carry explicit spans: `(a <-> b) | !z` matches both
/// everywhere-except-nothing and the two-word extent of `a <-> b`, and an
/// enclosing `<->` measures from that wider start. Hence `extra`.
///
/// A lexeme that is present but carries no positions has an *unknown* position
/// rather than no position: `'z:7 x' @@ '!x <2> z'` is false (x is there, so
/// `!x` holds nowhere) while `'z:7' @@ '!x <2> z'` is true (x is absent).
#[derive(Clone, Debug)]
enum PosSet {
    /// A finite set of spans, sorted and deduplicated. Empty means "nowhere".
    Exact(Vec<PMatch>),
    /// A match of width `width` ending at every position *except* `except`,
    /// together with the explicit spans in `extra`. Both lists are sorted and
    /// deduplicated. `width` is 0 for a plain negation; joining two negations
    /// yields a set whose members span the distance between them.
    Cofinite {
        except: Vec<i32>,
        width: i32,
        extra: Vec<PMatch>,
    },
    /// Present, but at an unknown position. Any phrase built on it is
    /// indeterminate, which `@@` reports as false.
    Unknown,
}

impl PosSet {
    /// Whether the subexpression definitely matches somewhere. A cofinite set is
    /// never empty.
    fn matches_anywhere(&self) -> bool {
        match self {
            PosSet::Exact(v) => !v.is_empty(),
            PosSet::Cofinite { .. } => true,
            PosSet::Unknown => false,
        }
    }
}

fn norm_points(mut v: Vec<i32>) -> Vec<i32> {
    v.sort_unstable();
    v.dedup();
    v
}

/// Sort, deduplicate, and drop any span reaching past the highest position a
/// tsvector can hold -- a negation placed there could never line up with a real
/// lexeme, and PG does not match it either (`'b:16382 y:16383' @@
/// 'b <-> (y <-> !x)'` is false, while the same shape one position lower is
/// true).
fn norm_spans(mut v: Vec<PMatch>) -> Vec<PMatch> {
    v.retain(|m| m.end <= i32::from(tsvector::MAX_POS));
    v.sort_unstable();
    v.dedup();
    v
}

/// The spans in `v` that begin at `p`. `v` is ordered by end, not start, so this
/// is a scan; `extra` only ever holds the spans an explicit `|` contributed.
fn starting_at(v: &[PMatch], p: i32) -> impl Iterator<Item = PMatch> + '_ {
    v.iter().copied().filter(move |m| m.start() == p)
}

/// The cofinite member *starting* at `p`, unless it is excluded.
fn cofinite_from(except: &[i32], width: i32, p: i32) -> Option<PMatch> {
    let end = p + width;
    except
        .binary_search(&end)
        .is_err()
        .then_some(PMatch { end, width })
}

/// The cofinite member *ending* at `e`, unless it is excluded.
fn cofinite_to(except: &[i32], width: i32, e: i32) -> Option<PMatch> {
    except
        .binary_search(&e)
        .is_err()
        .then_some(PMatch { end: e, width })
}

/// Keep whichever of two same-start spans reaches further, which is what an `&`
/// between phrase operands does.
fn longer(a: PMatch, b: PMatch) -> PMatch {
    if a.end >= b.end { a } else { b }
}

/// `tsvector @@ tsquery`.
pub fn matches(tv: &TsVector, q: &TsQuery) -> bool {
    match &q.root {
        None => false,
        Some(root) => eval(tv, root),
    }
}

/// Boolean evaluation, used everywhere outside a phrase operand.
fn eval(tv: &TsVector, node: &Node) -> bool {
    match node {
        // A lexeme matches if any of its positions passes the weight filter --
        // or if it has no positions at all, since a stripped vector carries no
        // weights to filter on (PG's long-standing behavior, which is why
        // `strip('wa:1A') @@ 'w:*D'` is true).
        Node::Val {
            word,
            prefix,
            weights,
        } => matching_lexemes(tv, word, *prefix).iter().any(|l| {
            l.positions.is_empty() || l.positions.iter().any(|p| weight_ok(*weights, p.weight))
        }),
        Node::Not(inner) => !eval(tv, inner),
        Node::And(l, r) => eval(tv, l) && eval(tv, r),
        Node::Or(l, r) => eval(tv, l) || eval(tv, r),
        Node::Phrase { .. } => phrase_eval(tv, node).matches_anywhere(),
    }
}

/// Positional evaluation, used for the operands of a phrase node.
fn phrase_eval(tv: &TsVector, node: &Node) -> PosSet {
    match node {
        Node::Val { word, prefix, .. } => {
            // *Any* positionless match makes the operand indeterminate, even if
            // another lexeme the prefix selects does carry positions: PG reports
            // `'b' 'bc':5 @@ 'b:* <0> bc'` as false, not true.
            if matching_lexemes(tv, word, *prefix)
                .iter()
                .any(|l| l.positions.is_empty())
            {
                return PosSet::Unknown;
            }
            // Otherwise: the positions that pass the weight filter, or a
            // definite "matches nowhere" when the term is absent or every
            // position was filtered out.
            PosSet::Exact(
                lexeme_positions(tv, node)
                    .into_iter()
                    .map(|p| PMatch::at(i32::from(p)))
                    .collect(),
            )
        }
        // `!!x` is `x`. Taking the complement twice would round the spans down
        // to points, so collapse the pair instead.
        Node::Not(inner) => match inner.as_ref() {
            Node::Not(x) => phrase_eval(tv, x),
            _ => complement(phrase_eval(tv, inner)),
        },
        Node::And(l, r) => intersect(phrase_eval(tv, l), phrase_eval(tv, r)),
        Node::Or(l, r) => union(phrase_eval(tv, l), phrase_eval(tv, r)),
        Node::Phrase { dist, left, right } => {
            join(phrase_eval(tv, left), phrase_eval(tv, right), i32::from(*dist))
        }
    }
}

/// `!x`: where `x` does not match. The exclusion is keyed on each span's **end**,
/// which is what makes `'a:1 b:2 c:3' @@ '!(a <-> b) <0> a'` true while
/// `… <0> b` is false.
fn complement(set: PosSet) -> PosSet {
    match set {
        PosSet::Unknown => PosSet::Unknown,
        PosSet::Exact(spans) => PosSet::Cofinite {
            except: norm_points(spans.iter().map(|s| s.end).collect()),
            width: 0,
            extra: Vec::new(),
        },
        // The complement of "matches anywhere but these" is "matches only at
        // these", minus the positions an explicit span already covers.
        //
        // Keyed on the excluded *end*. When the cofinite members span a distance
        // (a phrase between two negations, e.g. `!(!a <-> !b)`) neither this nor
        // keying on the start reproduces PG exactly; measured over 4400
        // differential cases against 18.4, end-keying leaves one divergence and
        // start-keying two, both only in phrases nesting three or more
        // negations. See `known_divergence_negated_negation_phrase`.
        PosSet::Cofinite { except, extra, .. } => PosSet::Exact(norm_spans(
            except
                .into_iter()
                .filter(|p| !extra.iter().any(|m| m.end == *p))
                .map(PMatch::at)
                .collect(),
        )),
    }
}

/// `a & b`: both operands must match starting at the same position, and PG keeps
/// the *longer* span -- which is what makes
/// `'a:1 b:2 c:3' @@ '((a <-> b) & a) <-> c'` true while `… <-> b` is false.
fn intersect(a: PosSet, b: PosSet) -> PosSet {
    match (a, b) {
        // A side that definitely matches nowhere settles the intersection even
        // if the other is unknown -- three-valued `NO & MAYBE` is `NO`.
        (PosSet::Exact(v), _) | (_, PosSet::Exact(v)) if v.is_empty() => PosSet::Exact(v),
        (PosSet::Unknown, _) | (_, PosSet::Unknown) => PosSet::Unknown,
        (PosSet::Exact(x), PosSet::Exact(y)) => {
            // Both sides are sorted by end, not start, so walk the shorter one
            // and probe the other by start.
            let mut out = Vec::new();
            for sx in &x {
                for sy in starting_at(&y, sx.start()) {
                    out.push(longer(*sx, sy));
                }
            }
            PosSet::Exact(norm_spans(out))
        }
        (
            PosSet::Exact(x),
            PosSet::Cofinite {
                except,
                width,
                extra,
            },
        )
        | (
            PosSet::Cofinite {
                except,
                width,
                extra,
            },
            PosSet::Exact(x),
        ) => {
            let mut out = Vec::new();
            for sx in &x {
                if let Some(c) = cofinite_from(&except, width, sx.start()) {
                    out.push(longer(*sx, c));
                }
                for y in starting_at(&extra, sx.start()) {
                    out.push(longer(*sx, y));
                }
            }
            PosSet::Exact(norm_spans(out))
        }
        (
            PosSet::Cofinite {
                except: e,
                width: we,
                extra: ex,
            },
            PosSet::Cofinite {
                except: f,
                width: wf,
                extra: fy,
            },
        ) => {
            let mut extra: Vec<PMatch> = Vec::new();
            for x in &ex {
                if let Some(c) = cofinite_from(&f, wf, x.start()) {
                    extra.push(longer(*x, c));
                }
                for y in starting_at(&fy, x.start()) {
                    extra.push(longer(*x, y));
                }
            }
            for y in &fy {
                if let Some(c) = cofinite_from(&e, we, y.start()) {
                    extra.push(longer(*y, c));
                }
            }
            // Two cofinite members share a start when neither side excludes its
            // own end for that start, and `&` keeps the longer -- so the result
            // has the wider extent, and an end is excluded when either side
            // excludes the end it would have had.
            let (wide, narrow) = if we >= wf { (we, wf) } else { (wf, we) };
            let (wide_except, narrow_except) = if we >= wf { (e, f) } else { (f, e) };
            let shift = wide - narrow;
            PosSet::Cofinite {
                except: norm_points(
                    wide_except
                        .into_iter()
                        .chain(narrow_except.into_iter().map(|q| q + shift))
                        .collect(),
                ),
                width: wide,
                extra: norm_spans(extra),
            }
        }
    }
}

fn union(a: PosSet, b: PosSet) -> PosSet {
    match (a, b) {
        (PosSet::Unknown, _) | (_, PosSet::Unknown) => PosSet::Unknown,
        (PosSet::Exact(x), PosSet::Exact(y)) => {
            PosSet::Exact(norm_spans(x.into_iter().chain(y).collect()))
        }
        // The explicit spans survive in `extra` -- that is what keeps
        // `(a <-> b) | !z` from collapsing to a point.
        (
            PosSet::Exact(x),
            PosSet::Cofinite {
                except,
                width,
                extra,
            },
        )
        | (
            PosSet::Cofinite {
                except,
                width,
                extra,
            },
            PosSet::Exact(x),
        ) => PosSet::Cofinite {
            except,
            width,
            extra: norm_spans(extra.into_iter().chain(x).collect()),
        },
        (
            PosSet::Cofinite {
                except: e,
                width: we,
                extra: ex,
            },
            PosSet::Cofinite {
                except: f,
                width: wf,
                extra: fy,
            },
        ) if we == wf => PosSet::Cofinite {
            // A cofinite member needs only one side, so it is excluded only when
            // both exclude it.
            except: e.into_iter().filter(|p| f.binary_search(p).is_ok()).collect(),
            width: we,
            extra: norm_spans(ex.into_iter().chain(fy).collect()),
        },
        // Different extents: keep the wider cofinite part and demote the other
        // side's explicit spans into `extra`.
        (
            PosSet::Cofinite {
                except: e,
                width: we,
                extra: ex,
            },
            PosSet::Cofinite {
                except: f,
                width: wf,
                extra: fy,
            },
        ) => {
            let (except, width, mut extra, other) = if we >= wf {
                (e, we, ex, fy)
            } else {
                (f, wf, fy, ex)
            };
            extra.extend(other);
            PosSet::Cofinite {
                except,
                width,
                extra: norm_spans(extra),
            }
        }
    }
}

/// `left <dist> right`: the left operand must end exactly `dist` before the right
/// operand starts. The result spans both, so a further `<->` measures from the
/// combined start.
fn join(left: PosSet, right: PosSet, dist: i32) -> PosSet {
    match (left, right) {
        // If either operand matches nowhere, so does the phrase -- even if the
        // other operand's position is unknown.
        (PosSet::Exact(v), _) | (_, PosSet::Exact(v)) if v.is_empty() => PosSet::Exact(v),
        (PosSet::Unknown, _) | (_, PosSet::Unknown) => PosSet::Unknown,
        (PosSet::Exact(ls), PosSet::Exact(rs)) => {
            let mut out = Vec::new();
            for r in &rs {
                // The left operand's end is pinned by this right span, so probe
                // the sorted left side instead of scanning it.
                let want = r.start() - dist;
                let from = ls.partition_point(|l| l.end < want);
                for l in ls[from..].iter().take_while(|l| l.end == want) {
                    out.push(l.spanning(*r));
                }
            }
            PosSet::Exact(norm_spans(out))
        }
        (
            PosSet::Cofinite {
                except,
                width,
                extra,
            },
            PosSet::Exact(rs),
        ) => {
            let mut out = Vec::new();
            for r in &rs {
                let want = r.start() - dist;
                if let Some(l) = cofinite_to(&except, width, want) {
                    out.push(l.spanning(*r));
                }
                for l in extra.iter().filter(|l| l.end == want) {
                    out.push(l.spanning(*r));
                }
            }
            PosSet::Exact(norm_spans(out))
        }
        (
            PosSet::Exact(ls),
            PosSet::Cofinite {
                except,
                width,
                extra,
            },
        ) => {
            let mut out = Vec::new();
            for l in &ls {
                let want = l.end + dist;
                if let Some(r) = cofinite_from(&except, width, want) {
                    out.push(l.spanning(r));
                }
                for r in starting_at(&extra, want) {
                    out.push(l.spanning(r));
                }
            }
            PosSet::Exact(norm_spans(out))
        }
        (
            PosSet::Cofinite {
                except: e,
                width: we,
                extra: ex,
            },
            PosSet::Cofinite {
                except: f,
                width: wf,
                extra: fy,
            },
        ) => {
            // A member ending at `x` needs the right side to allow `x` and the
            // left side to allow the position `wf + dist` before it.
            let mut except: Vec<i32> = f.clone();
            except.extend(e.iter().map(|p| p + wf + dist));
            let mut extra = Vec::new();
            for r in &fy {
                let want = r.start() - dist;
                if let Some(l) = cofinite_to(&e, we, want) {
                    extra.push(l.spanning(*r));
                }
            }
            for l in &ex {
                let want = l.end + dist;
                if let Some(r) = cofinite_from(&f, wf, want) {
                    extra.push(l.spanning(r));
                }
                for r in starting_at(&fy, want) {
                    extra.push(l.spanning(r));
                }
            }
            PosSet::Cofinite {
                except: norm_points(except),
                width: we + dist + wf,
                extra: norm_spans(extra),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tsquery;
    use crate::tsvector::tsvector_in;

    /// Parse and re-emit in canonical form.
    fn round(s: &str) -> Result<String, TsError> {
        Ok(format(&q(s)?))
    }

    fn q(s: &str) -> Result<TsQuery, TsError> {
        tsquery_in(s)
    }

    /// `vector @@ query`, parsing both sides.
    fn m(vector: &str, query: &str) -> Result<bool, TsError> {
        Ok(matches(&tsvector_in(vector)?, &q(query)?))
    }

    #[test]
    fn basic_io() -> Result<(), TsError> {
        assert_eq!(round(" 1 ")?, "'1'");
        assert_eq!(round("'1 2'")?, "'1 2'");
        assert_eq!(round(r"'1 \'2'")?, "'1 ''2'");
        assert_eq!(round("!1")?, "!'1'");
        assert_eq!(round("1|2")?, "'1' | '2'");
        assert_eq!(round("1&2")?, "'1' & '2'");
        assert_eq!(round(r"'\\as'")?, r"'\\as'");
        // A trailing bare colon is accepted and means "no restriction".
        assert_eq!(round("a:")?, "'a'");
        assert_eq!(q("")?, TsQuery::default());
        assert_eq!(format(&TsQuery::default()), "");
        Ok(())
    }

    #[test]
    fn precedence_and_parens() -> Result<(), TsError> {
        // `&` binds tighter than `|`, so only the loose child needs parens.
        assert_eq!(round("1|2&3")?, "'1' | '2' & '3'");
        assert_eq!(round("(1|2)&3")?, "( '1' | '2' ) & '3'");
        assert_eq!(round("!(1|2)&3")?, "!( '1' | '2' ) & '3'");
        assert_eq!(round("(!1|2)&3")?, "( !'1' | '2' ) & '3'");
        assert_eq!(round("1&(2&(4&(5|6)))")?, "'1' & '2' & '4' & ( '5' | '6' )");
        // `!` binds tightest, and stacks without parens.
        assert_eq!(round("!!b")?, "!!'b'");
        assert_eq!(round("!(!b)")?, "!!'b'");
        assert_eq!(round("a & !!b")?, "'a' & !!'b'");
        assert_eq!(round("!(a&b)")?, "!( 'a' & 'b' )");
        // Phrase binds tighter than `&`/`|` but looser than `!`.
        assert_eq!(round("!a <-> b")?, "!'a' <-> 'b'");
        assert_eq!(round("(a<->b)&c")?, "'a' <-> 'b' & 'c'");
        assert_eq!(round("a<->(b|c)")?, "'a' <-> ( 'b' | 'c' )");
        assert_eq!(round("(a&b)<->c")?, "( 'a' & 'b' ) <-> 'c'");
        // A phrase's right operand is parenthesized when it is a phrase too;
        // the left one is not.
        assert_eq!(round("a<->b<->c")?, "'a' <-> 'b' <-> 'c'");
        assert_eq!(round("a<->(b<->c)")?, "'a' <-> ( 'b' <-> 'c' )");
        assert_eq!(
            round("(a<->b)<->(c<->d)")?,
            "'a' <-> 'b' <-> ( 'c' <-> 'd' )"
        );
        assert_eq!(round("a<2>b")?, "'a' <2> 'b'");
        assert_eq!(round("a<0>b")?, "'a' <0> 'b'");
        Ok(())
    }

    #[test]
    fn weights_and_prefix() -> Result<(), TsError> {
        assert_eq!(
            round("a:* & nbb:*ac | doo:a* | goo")?,
            "'a':* & 'nbb':*AC | 'doo':*A | 'goo'"
        );
        // Weight letters are a set: any order in, canonical ABCD order out.
        assert_eq!(round("a:BA")?, "'a':AB");
        assert_eq!(round("a:dcba")?, "'a':ABCD");
        assert_eq!(round("a:*D")?, "'a':*D");
        assert_eq!(round("a:*b")?, "'a':*B");
        Ok(())
    }

    #[test]
    fn tree_shape_is_significant() -> Result<(), TsError> {
        // Distinct values that render identically, matching PG.
        assert_ne!(q("1|2|4")?, q("1|(2|4)")?);
        assert_ne!(q("1&2&4")?, q("1&(2&4)")?);
        assert_eq!(round("1|2|4|5|6")?, round("1|(2|(4|(5|6)))")?);
        Ok(())
    }

    #[test]
    fn syntax_errors() -> Result<(), TsError> {
        for bad in [
            "a b", "& a", "()", "(a", "a)", "a<->", "a <2>", "a !", "<->a", "a:x", "a&&b", "a||b",
        ] {
            let err = tsquery_in(bad).expect_err("rejects");
            assert_eq!(err.sqlstate, SYNTAX_ERROR, "{bad}");
            assert_eq!(err.message, format!("syntax error in tsquery: \"{bad}\""));
        }
        // Running out of input where an operand belongs is reported differently
        // from other syntax errors.
        for bad in ["a &", "a |", "!", "a & !", "a & ("] {
            let err = tsquery_in(bad).expect_err("rejects");
            assert_eq!(err.sqlstate, SYNTAX_ERROR, "{bad}");
            assert_eq!(err.message, format!("no operand in tsquery: \"{bad}\""));
        }
        // The phrase distance has its own range error.
        let err = tsquery_in("a <100000> b").expect_err("rejects");
        assert_eq!(err.sqlstate, INVALID_PARAMETER_VALUE);
        assert_eq!(
            err.message,
            "distance in phrase operator must be an integer value between zero and 16384 inclusive"
        );
        assert_eq!(round("a <16384> b")?, "'a' <16384> 'b'");
        Ok(())
    }

    #[test]
    fn deep_nesting_is_bounded_not_a_stack_overflow() -> Result<(), TsError> {
        // A pathological literal must return an error, not abort the backend --
        // whether it nests by recursion (`!`, parens) or builds a deep left
        // spine through the parser's *loops* (`a&a&a&...`), which cost no parse
        // stack but produce a tree every later walk recurses over.
        for deep in [
            format!("{}a", "!".repeat(MAX_PARSE_DEPTH + 5)),
            format!(
                "{}a{}",
                "(".repeat(MAX_PARSE_DEPTH + 5),
                ")".repeat(MAX_PARSE_DEPTH + 5)
            ),
            format!("{}a", "a&".repeat(MAX_NODE_DEPTH + 5)),
            format!("{}a", "a|".repeat(MAX_NODE_DEPTH + 5)),
            format!("{}a", "a<->".repeat(MAX_NODE_DEPTH + 5)),
        ] {
            let err = tsquery_in(&deep).expect_err("rejects over-deep input");
            assert_eq!(err.sqlstate, PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(err.message, "tsquery stack too small");
        }
        // Just inside the limit still parses, and every later walk over the tree
        // (format, cmp, numnode, matching) stays within the same bound.
        let ok = format!(
            "{}a{}",
            "(".repeat(MAX_PARSE_DEPTH - 1),
            ")".repeat(MAX_PARSE_DEPTH - 1)
        );
        let parsed = q(&ok)?;
        assert_eq!(format(&parsed), "'a'");
        assert_eq!(numnode(&parsed), 1);
        assert_eq!(cmp(&parsed, &parsed), Ordering::Equal);
        let nots = format!("{}a", "!".repeat(MAX_PARSE_DEPTH - 1));
        let parsed = q(&nots)?;
        assert!(!tsquery::matches(&tsvector_in("a")?, &parsed));
        // A spine just inside the node cap parses and survives every walk --
        // including `decode`, the deepest one, which is what the cap is sized
        // against.
        let spine = format!("{}a", "a&".repeat(MAX_NODE_DEPTH - 1));
        let parsed = q(&spine)?;
        assert_eq!(decode(&encode(&parsed)).as_ref(), Some(&parsed));
        assert_eq!(numnode(&parsed), (MAX_NODE_DEPTH as i32 - 1) * 2 + 1);
        assert!(tsquery::matches(&tsvector_in("a")?, &parsed));
        assert_eq!(cmp(&parsed, &parsed), Ordering::Equal);
        assert!(!format(&parsed).is_empty());
        Ok(())
    }

    #[test]
    fn numnode_and_querytree() -> Result<(), TsError> {
        assert_eq!(numnode(&q("new")?), 1);
        assert_eq!(numnode(&q("new & york")?), 3);
        assert_eq!(numnode(&q("new & york | qwery")?), 5);
        assert_eq!(numnode(&q("a <-> b")?), 3);
        assert_eq!(numnode(&q("!!a")?), 3);
        assert_eq!(numnode(&TsQuery::default()), 0);

        assert_eq!(querytree(&q("foo & ! bar")?), "'foo'");
        assert_eq!(querytree(&q("!foo")?), "T");
        assert_eq!(querytree(&q("a|!b")?), "T");
        assert_eq!(querytree(&q("a<->!b")?), "'a'");
        assert_eq!(querytree(&q("a&b")?), "'a' & 'b'");
        assert_eq!(querytree(&q("!a&!b")?), "T");
        assert_eq!(querytree(&q("(a|b)&!c")?), "'a' | 'b'");
        assert_eq!(querytree(&q("a&(b|!c)")?), "'a'");
        Ok(())
    }

    #[test]
    fn combinators() -> Result<(), TsError> {
        assert_eq!(
            format(&and(&q("foo & bar")?, &q("asd")?)?),
            "'foo' & 'bar' & 'asd'"
        );
        assert_eq!(
            format(&or(&q("foo & bar")?, &q("asd & fg")?)?),
            "'foo' & 'bar' | 'asd' & 'fg'"
        );
        assert_eq!(
            format(&or(&q("foo & bar")?, &not(&q("asd & fg")?)?)?),
            "'foo' & 'bar' | !( 'asd' & 'fg' )"
        );
        assert_eq!(
            format(&and(&q("foo & bar")?, &q("asd | fg")?)?),
            "'foo' & 'bar' & ( 'asd' | 'fg' )"
        );
        assert_eq!(
            format(&phrase(&q("a")?, &q("b & d")?, 1)?),
            "'a' <-> ( 'b' & 'd' )"
        );
        assert_eq!(
            format(&phrase(&q("a <3> g")?, &q("b & d")?, 10)?),
            "'a' <3> 'g' <10> ( 'b' & 'd' )"
        );
        // An empty operand drops out rather than producing an empty result.
        assert_eq!(format(&and(&q("a")?, &TsQuery::default())?), "'a'");
        // Growing a query past the node cap is refused rather than building a
        // tree later walks cannot recurse over.
        let mut deep = q("a")?;
        for _ in 0..MAX_NODE_DEPTH {
            match and(&deep, &q("b")?) {
                Ok(next) => deep = next,
                Err(e) => {
                    assert_eq!(e.message, "tsquery stack too small");
                    return Ok(());
                }
            }
        }
        panic!("expected the node-depth cap to stop the loop");
    }

    #[test]
    fn total_order() -> Result<(), TsError> {
        // Node count dominates.
        assert_eq!(cmp(&q("a")?, &q("b & c")?), Ordering::Less);
        // Then total lexeme bytes: `a|ff` (3 bytes) outranks `b&c` (2).
        assert_eq!(cmp(&q("a|ff")?, &q("b & c")?), Ordering::Greater);
        assert_eq!(cmp(&q("a|f|g")?, &q("b & c")?), Ordering::Greater);
        // Then the operator: a phrase sorts before `|`, which sorts before `&`.
        assert_eq!(cmp(&q("a|f")?, &q("b & c")?), Ordering::Less);
        assert_eq!(cmp(&q("a<->b")?, &q("a & b")?), Ordering::Less);
        assert_eq!(cmp(&q("a")?, &q("a")?), Ordering::Equal);
        Ok(())
    }

    #[test]
    fn boolean_matching() -> Result<(), TsError> {
        assert!(m("a b:89 ca:23A,64b d:34c", "d:AC & ca")?);
        assert!(m("a b:89 ca:23A,64b d:34c", "d:AC & ca:B")?);
        assert!(!m("a b:89 ca:23A,64b d:34c", "d:AC & ca:C")?);
        assert!(m("a b:89 ca:23A,64b d:34c", "d:AC & ca:CB")?);
        assert!(!m("a b:89 ca:23A,64b d:34c", "d:AC & c:*C")?);
        assert!(m("a b:89 ca:23A,64b d:34c", "d:AC & c:*CB")?);
        // Prefix matching.
        assert!(!m("supernova", "super")?);
        assert!(m("supernova", "super:*")?);
        assert!(m("supeanova supernova", "super:*")?);
        // Negation.
        assert!(!m("wa:1A", "!w:*A")?);
        assert!(m("wa:1A", "!w:*D")?);
        // An empty query matches nothing.
        assert!(!m("a b c", "")?);
        Ok(())
    }

    #[test]
    fn stripped_vectors_ignore_weights() -> Result<(), TsError> {
        // A vector with no positions has no weights either, so weight filters
        // are ignored rather than failing.
        assert!(m("'wa'", "w:*A")?);
        assert!(m("'wa'", "w:*D")?);
        assert!(!m("'wa'", "!w:*A")?);
        assert!(!m("'wa'", "!w:*D")?);
        // ... and it can never satisfy a phrase.
        assert!(!m("x y q y", "!x <-> y")?);
        assert!(m("x y q y", "!(x <-> y)")?);
        Ok(())
    }

    #[test]
    fn phrase_matching() -> Result<(), TsError> {
        assert!(m("a:1 b:2", "a <-> b")?);
        assert!(!m("a:1 b:2", "a <0> b")?);
        assert!(m("a:1 b:2", "a <1> b")?);
        assert!(!m("a:1 b:2", "a <2> b")?);
        assert!(m("a:1 b:3", "a <2> b")?);
        assert!(m("a:1 b:3", "a <0> a:*")?);
        assert!(m("wa:1D wb:2A", "w:*D <-> w:*A")?);
        assert!(!m("wa:1A wb:2D", "w:*D <-> w:*A")?);
        // Chained phrases measure from the combined span, which is why a
        // right-nested phrase still matches from the outer operand's position.
        assert!(m("1:1 2:2 3:3 4:4", "1 <-> 2 <-> 3")?);
        assert!(m("1:1 2:2 3:3 4:4", "(1 <-> 2) <-> 3")?);
        assert!(m("1:1 2:2 3:3 4:4", "1 <-> (2 <-> 3)")?);
        assert!(!m("1:1 2:2 3:3 4:4", "1 <2> (2 <-> 3)")?);
        Ok(())
    }

    /// The one shape where the phrase evaluator still disagrees with PG.
    /// Negating a phrase whose *operands* are themselves negations produces a
    /// match set with no exact representation here; both available spellings of
    /// `complement` get some case wrong. Pinned so the gap is explicit, and so a
    /// future fix has a failing target to aim at.
    #[test]
    fn known_divergence_negated_negation_phrase() -> Result<(), TsError> {
        // PostgreSQL 18.4 answers `true`; we answer `false`. The `&` mixes a
        // point-wide negation with a phrase-wide one, and negating the result
        // has to place a point somewhere in that span.
        assert!(!m("x:3A,1A,7", "x <2> !(!x & (!a <3> !b))")?);
        // A negated phrase between two negations, one level shallower, agrees.
        assert!(m("y:5 b:8B z:2B,7A", "(!(!z <-> !c) <-> (!x <3> z:*)) <-> !z:*")?);
        // Everything one level simpler agrees -- a single negation under a
        // phrase, and a negated phrase over plain lexemes, are both exact.
        assert!(m("a:1", "!b <-> a")?);
        assert!(m("a:1 b:2 c:3", "!(a <-> b) <0> a")?);
        assert!(!m("a:1 b:2 c:3", "!(a <-> b) <0> b")?);
        Ok(())
    }

    #[test]
    fn phrase_operands_are_position_sets() -> Result<(), TsError> {
        // `&` intersects and `|` unions the operand position sets.
        assert!(!m("q:1 x:2 q:3 y:4", "q <-> (x & y)")?);
        assert!(m("q:1 x:2", "q <-> (x | y <-> z)")?);
        assert!(!m("q:1 y:2", "q <-> (x | y <-> z)")?);
        assert!(m("q:1 y:2 z:3", "q <-> (x | y <-> z)")?);
        assert!(!m("q:1 y:2 x:3", "q <-> (x | y <-> z)")?);
        assert!(m("q:1 x:2 y:3", "q <-> (x | y <-> z)")?);
        assert!(!m("q:1 x:2", "(x | y <-> z) <-> q")?);
        assert!(m("x:1 q:2", "(x | y <-> z) <-> q")?);
        assert!(m("x:1 y:2 z:3 q:4", "(x | y <-> z) <-> q")?);
        assert!(m("y:1 z:2 q:3", "(x | y <-> z) <-> q")?);
        assert!(!m("y:1 y:2 q:3", "(x | y <-> z) <-> q")?);
        // `!` inside a phrase matches at any position the negated term does not
        // occupy -- unbounded, so it can sit one before the vector's first
        // lexeme or one past its last. Every case below was checked against
        // PostgreSQL 18.4.
        assert!(m("a:1", "!b <-> a")?);
        assert!(m("a:1 b:5", "b <-> !z")?);
        assert!(m("a:1", "a <-> !b")?);
        assert!(m("a:1", "!b <-> !c")?);
        assert!(m("a:1", "!a <-> a")?);
        assert!(m("", "!foo <-> !bar")?);
        // ... but a vector with no positions at all can never satisfy a phrase.
        let stripped = tsvector::strip(&tsvector::tsvector_in("a:1")?);
        assert!(!matches(&stripped, &tsquery_in("!b <-> a")?));
        // `&` between phrase operands requires a shared start and keeps the
        // longer span, so the enclosing `<->` measures from the wider extent.
        assert!(m("a:1 b:2 c:3", "((a <-> b) & a) <-> c")?);
        assert!(m("a:1 b:2 c:3", "a <-> (b & (b <-> c))")?);
        assert!(!m("a:1 b:2 c:3", "((a <-> b) & a) <-> b")?);
        assert!(m("y:1 y:2 q:3", "(!x | y <-> z) <-> q")?);
        assert!(!m("x:1 q:2", "(!x | y <-> z) <-> q")?);
        assert!(m("z:1 q:2", "(!x | y <-> z) <-> q")?);
        assert!(!m("x:1 y:2 q:3", "(!x | y) <-> y <-> q")?);
        assert!(m("x:1 y:2 q:3", "(!x | !y) <-> y <-> q")?);
        assert!(m("x:1 y:2 q:3", "(x | !y) <-> y <-> q")?);
        assert!(m("x:1 y:2 q:3", "(x | !!z) <-> y <-> q")?);
        assert!(m("x:1 y:2 q:3 y:4", "!x <-> y")?);
        assert!(m("x:1 y:2 q:3 y:4", "!x <-> !y")?);
        assert!(!m("x:1 y:2 q:3 y:4", "!(x <-> y)")?);
        assert!(m("x:1 y:2 q:3 y:4", "!(x <2> y)")?);
        // A query no lexeme contradicts matches even an empty vector.
        assert!(m("x:1 y:2 q:3 y:4", "!foo")?);
        assert!(m("", "!foo")?);
        Ok(())
    }
}
