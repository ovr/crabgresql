//! Array I/O (`array_out` / `array_in`) and the element↔array OID mapping.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable* array
//! text format — the `{...}` syntax, nested braces for the higher dimensions,
//! the `[lower:upper]=` dimension prefix, the quoting/escaping rules, and the
//! case-insensitive unquoted `NULL` element — implemented independently.
//!
//! Arrays are *not* nested values: an array has a dimension list and a single
//! flat, row-major element list, which is why `int[]` and `int[][]` are the same
//! type (`_int4`) and dimensionality belongs to the value. See [`ArrayDim`].

use crate::{FmtCtx, PgType, Value, cast, oid};

/// The most dimensions an array value may have. PostgreSQL's `MAXDIM`; a
/// literal that nests deeper is rejected with `54000`.
pub const MAXDIM: usize = 6;

/// One dimension of an array: its lower subscript bound and its length.
///
/// `lower` is 1 for every array PostgreSQL builds itself, but a literal may fix
/// another one (`'[2:3]={1,2}'::int[]`), and that bound is part of the value —
/// it shifts subscripts, prints back out as a `[lower:upper]=` prefix, and makes
/// the value unequal to the same elements at the default bound.
///
/// Deliberately **not** `Ord`. A field-order comparison would read as the
/// obvious way to break a tie between two shapes, and it is the wrong one: PG
/// weighs every dimension's length before any lower bound, which no per-dimension
/// ordering can express. `compare::compare_values` spells that out.
#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayDim {
    pub lower: i32,
    pub len: i32,
}

impl ArrayDim {
    /// A dimension of `len` elements at the default lower bound of 1.
    pub fn new(len: usize) -> Self {
        ArrayDim {
            lower: 1,
            len: len as i32,
        }
    }

    /// The highest valid subscript — what `array_upper` reports.
    pub fn upper(&self) -> i32 {
        // Both bounds are held to `i32` range by `array_in`, so this cannot wrap
        // for a value that exists.
        self.lower + self.len - 1
    }
}

/// The dimension list of a 1-D array of `len` elements, or the empty list when
/// `len` is 0 — an empty array has *no* dimensions in PostgreSQL, which is why
/// `array_dims('{}'::int[])` and `array_length('{}'::int[], 1)` are both NULL.
pub fn dims_1d(len: usize) -> Vec<ArrayDim> {
    if len == 0 {
        Vec::new()
    } else {
        vec![ArrayDim::new(len)]
    }
}

/// Stack the operands of a nested `ARRAY[…]` constructor
/// (`ARRAY[[1,2],[3,4]]`, `ARRAY[ARRAY[…]]`) into one array with one more
/// dimension.
///
/// The operands are already row-major, so the result's elements are simply
/// theirs concatenated; only the dimension list grows, by the operand count at
/// the front.
///
/// Two ways to fail, and PostgreSQL words them differently: operands that
/// disagree about their shape are `2202E`, and stacking past [`MAXDIM`] is
/// `54000` — the *constructor's* wording of that limit names the offending
/// count, where the literal parser's does not.
pub fn stack(elem: PgType, operands: &[Value]) -> Result<Value, ArrayError> {
    let ragged = || ArrayError {
        sqlstate: ARRAY_SUBSCRIPT_ERROR,
        message: "multidimensional arrays must have array expressions with matching dimensions"
            .to_string(),
        detail: None,
    };
    let mut dims: Option<&[ArrayDim]> = None;
    let mut elems = Vec::new();
    for operand in operands {
        // A NULL operand has no shape to stack, so it cannot take part.
        let Value::Array {
            dims: sub,
            elems: e,
            ..
        } = operand
        else {
            return Err(ragged());
        };
        match dims {
            None => dims = Some(sub),
            Some(established) if established == sub => {}
            Some(_) => return Err(ragged()),
        }
        elems.extend(e.iter().cloned());
    }
    let mut dims = dims.unwrap_or_default().to_vec();
    // Every operand was itself empty, so the whole constructor is: no dimensions.
    if !dims.is_empty() {
        if dims.len() >= MAXDIM {
            return Err(too_many_dimensions_n(dims.len() + 1));
        }
        dims.insert(0, ArrayDim::new(operands.len()));
    }
    Ok(Value::Array { elem, dims, elems })
}

/// SQLSTATE + message (+ optional DETAIL) for a failed array input (`array_in`).
/// The DETAIL is either one of PG's `array_in` lines (e.g. `Unexpected ","
/// character.`) or whatever the failing element's own input function reported,
/// which is why it is owned rather than `&'static str`.
#[derive(Clone, Debug, PartialEq)]
pub struct ArrayError {
    pub sqlstate: &'static str,
    pub message: String,
    pub detail: Option<String>,
}

const INVALID_TEXT_REPRESENTATION: &str = "22P02";

fn malformed(s: &str, detail: &'static str) -> ArrayError {
    ArrayError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("malformed array literal: \"{s}\""),
        detail: Some(detail.to_string()),
    }
}

// PG's `array_in` DETAIL strings.
const DETAIL_START: &str = "Array value must start with \"{\" or dimension information.";
const DETAIL_EOF: &str = "Unexpected end of input.";
const DETAIL_JUNK: &str = "Junk after closing right brace.";
const DETAIL_COMMA: &str = "Unexpected \",\" character.";
/// `box` is the one built-in whose `typdelim` is `;` rather than `,`; its
/// element text (`(1,1),(0,0)`) is full of commas, so the array delimiter has
/// to differ. PG's DETAIL then names `;` instead.
const DETAIL_SEMICOLON: &str = "Unexpected \";\" character.";

/// The DETAIL naming whichever delimiter this element type uses.
const fn detail_delim(delim: char) -> &'static str {
    if delim == ';' {
        DETAIL_SEMICOLON
    } else {
        DETAIL_COMMA
    }
}
const DETAIL_RBRACE: &str = "Unexpected \"}\" character.";
const DETAIL_LBRACE: &str = "Unexpected \"{\" character.";
/// Sub-arrays that disagree about their shape, either in depth (`{{1,2},3}`) or
/// in length (`{{1,2},{3}}`).
const DETAIL_RAGGED: &str =
    "Multidimensional arrays must have sub-arrays with matching dimensions.";
const DETAIL_NO_EQUALS: &str = "Missing \"=\" after array dimensions.";
/// A bracket pair that never closes (`'[2:3'`).
const DETAIL_NO_RBRACKET: &str = "Missing \"]\" after array dimensions.";
/// A bracket pair holding something other than a bound (or nothing at all):
/// `'[a:b]=…'`, `'[]=…'`, `'[ 2 : 3 ]=…'`.
const DETAIL_BAD_DIMS: &str = "\"[\" must introduce explicitly-specified array dimensions.";
/// A complete `[l:u]…=` prefix with no `{` after it. Once the dimensions have
/// been read PG stops offering them as an alternative, so this is a *different*
/// line from [`DETAIL_START`].
const DETAIL_NO_CONTENTS: &str = "Array contents must start with \"{\".";
/// A `[l:u]…=` prefix that disagrees with the braces after it, in either the
/// number of dimensions or a length.
const DETAIL_DIM_MISMATCH: &str = "Specified array dimensions do not match array contents.";

/// `54000` (`program_limit_exceeded`) — more brace levels than [`MAXDIM`].
const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
/// `2202E` (`array_subscript_error`) — a `[l:u]` prefix whose upper bound is
/// below its lower bound. Note this is *not* a malformed literal to PostgreSQL:
/// the syntax parsed fine, the bounds it names are the problem.
const ARRAY_SUBSCRIPT_ERROR: &str = "2202E";

/// The array type OID for an element type OID (`int4` → `_int4` = 1007), or
/// `None` when this build has no array type for that element.
pub fn array_oid_for_elem(elem: u32) -> Option<u32> {
    ARRAY_OID_PAIRS
        .iter()
        .find(|(e, _)| *e == elem)
        .map(|(_, a)| *a)
}

/// The element type OID for an array type OID (`_int4` = 1007 → `int4`), the
/// reverse of [`array_oid_for_elem`]. Used by [`PgType::from_oid`] to decode a
/// declared array parameter OID.
pub fn elem_oid_for_array(arr: u32) -> Option<u32> {
    ARRAY_OID_PAIRS
        .iter()
        .find(|(_, a)| *a == arr)
        .map(|(e, _)| *e)
}

/// Every (element OID, array OID) pair this build models, for callers that need
/// to sweep them rather than look one up — the drift tests against `pg_type` in
/// `crabgresql-catalog`, and the name round-trip here.
pub fn pairs() -> impl Iterator<Item = (u32, u32)> {
    ARRAY_OID_PAIRS.iter().copied()
}

/// (element OID, array OID) pairs, matching PostgreSQL's `pg_type.typarray`.
const ARRAY_OID_PAIRS: &[(u32, u32)] = &[
    (oid::BOOL, oid::BOOL_ARRAY),
    (oid::BYTEA, oid::BYTEA_ARRAY),
    (oid::NAME, oid::NAME_ARRAY),
    (oid::INT2, oid::INT2_ARRAY),
    (oid::INT4, oid::INT4_ARRAY),
    (oid::INT8, oid::INT8_ARRAY),
    (oid::TEXT, oid::TEXT_ARRAY),
    (oid::VARCHAR, oid::VARCHAR_ARRAY),
    (oid::BPCHAR, oid::BPCHAR_ARRAY),
    (oid::CHAR, oid::CHAR_ARRAY),
    (oid::OID, oid::OID_ARRAY),
    (oid::TID, oid::TID_ARRAY),
    (oid::XID, oid::XID_ARRAY),
    (oid::XID8, oid::XID8_ARRAY),
    (oid::CID, oid::CID_ARRAY),
    (oid::PG_LSN, oid::PG_LSN_ARRAY),
    (oid::FLOAT4, oid::FLOAT4_ARRAY),
    (oid::FLOAT8, oid::FLOAT8_ARRAY),
    (oid::NUMERIC, oid::NUMERIC_ARRAY),
    (oid::MONEY, oid::MONEY_ARRAY),
    (oid::POINT, oid::POINT_ARRAY),
    (oid::LSEG, oid::LSEG_ARRAY),
    (oid::PATH, oid::PATH_ARRAY),
    (oid::BOX, oid::BOX_ARRAY),
    (oid::POLYGON, oid::POLYGON_ARRAY),
    (oid::LINE, oid::LINE_ARRAY),
    (oid::CIRCLE, oid::CIRCLE_ARRAY),
    (oid::MACADDR, oid::MACADDR_ARRAY),
    (oid::MACADDR8, oid::MACADDR8_ARRAY),
    (oid::INET, oid::INET_ARRAY),
    (oid::CIDR, oid::CIDR_ARRAY),
    (oid::UUID, oid::UUID_ARRAY),
    (oid::JSON, oid::JSON_ARRAY),
    (oid::JSONB, oid::JSONB_ARRAY),
    (oid::JSONPATH, oid::JSONPATH_ARRAY),
    (oid::TSVECTOR, oid::TSVECTOR_ARRAY),
    (oid::TSQUERY, oid::TSQUERY_ARRAY),
    (oid::DATE, oid::DATE_ARRAY),
    (oid::TIME, oid::TIME_ARRAY),
    (oid::TIMETZ, oid::TIMETZ_ARRAY),
    (oid::TIMESTAMP, oid::TIMESTAMP_ARRAY),
    (oid::TIMESTAMPTZ, oid::TIMESTAMPTZ_ARRAY),
    (oid::INTERVAL, oid::INTERVAL_ARRAY),
    (oid::BIT, oid::BIT_ARRAY),
    (oid::VARBIT, oid::VARBIT_ARRAY),
    (oid::REGPROC, oid::REGPROC_ARRAY),
    (oid::REGPROCEDURE, oid::REGPROCEDURE_ARRAY),
    (oid::REGOPER, oid::REGOPER_ARRAY),
    (oid::REGOPERATOR, oid::REGOPERATOR_ARRAY),
    (oid::REGCLASS, oid::REGCLASS_ARRAY),
    (oid::REGTYPE, oid::REGTYPE_ARRAY),
    (oid::REGNAMESPACE, oid::REGNAMESPACE_ARRAY),
    // PG treats the vectors as scalars for array construction, so `oidvector[]`
    // is an array *of vectors*, not a flattened `oid[]`.
    (oid::OIDVECTOR, oid::OIDVECTOR_ARRAY),
    (oid::INT2VECTOR, oid::INT2VECTOR_ARRAY),
];

/// `array_out`: render an array as `{e1,e2,...}`, one brace level per dimension
/// (`{{1,2},{3,4}}` for a 2×2). A NULL element prints as an unquoted `NULL`; any
/// other element is rendered with its own output function and double-quoted when
/// it is empty, equals `NULL` case-insensitively, or contains a delimiter, brace,
/// quote, backslash, or whitespace.
///
/// When *any* dimension has a lower bound other than 1, the whole value is
/// prefixed with its dimensions (`[2:3][0:1]={{1,2},{3,4}}`) so that the text
/// round-trips through [`array_in`]; at the default bounds the prefix is omitted.
pub fn format(elem: PgType, dims: &[ArrayDim], elems: &[Value], fmt: &FmtCtx) -> String {
    if dims.is_empty() {
        return String::from("{}");
    }
    let delim = elem.typdelim();
    let mut out = String::new();
    if dims.iter().any(|d| d.lower != 1) {
        for d in dims {
            out.push_str(&format!("[{}:{}]", d.lower, d.upper()));
        }
        out.push('=');
    }
    format_slice(&mut out, dims, elems, delim, fmt);
    out
}

/// The 1-D form, for callers that build their arrays flat and never carry a
/// dimension list of their own.
pub fn format_1d(elem: PgType, elems: &[Value], fmt: &FmtCtx) -> String {
    format(elem, &dims_1d(elems.len()), elems, fmt)
}

/// Emit one brace group. `dims` is the *remaining* dimension list, `elems` the
/// row-major slice this group covers; the recursion splits that slice into
/// `dims[0].len` equal chunks, one per sub-group.
fn format_slice(out: &mut String, dims: &[ArrayDim], elems: &[Value], delim: char, fmt: &FmtCtx) {
    out.push('{');
    match dims.split_first() {
        // Innermost level: the elements themselves.
        Some((_, [])) => {
            for (i, v) in elems.iter().enumerate() {
                if i > 0 {
                    out.push(delim);
                }
                match v {
                    Value::Null => out.push_str("NULL"),
                    _ => {
                        let s = v.encode_text_with(fmt).unwrap_or_default();
                        if needs_quote(&s, delim) {
                            push_quoted(out, &s);
                        } else {
                            out.push_str(&s);
                        }
                    }
                }
            }
        }
        Some((first, rest)) => {
            let stride = rest.iter().map(|d| d.len as usize).product::<usize>();
            for i in 0..first.len as usize {
                if i > 0 {
                    out.push(delim);
                }
                let from = i * stride;
                format_slice(out, rest, &elems[from..from + stride], delim, fmt);
            }
        }
        None => {}
    }
    out.push('}');
}

/// C's `isspace` over ASCII — the six characters PG's `array_isspace` and
/// `oidvectorin` both treat as whitespace.
///
/// Deliberately neither Rust's Unicode-aware `char::is_whitespace` (which would
/// over-quote an element containing e.g. a non-breaking space) nor
/// `is_ascii_whitespace` (which omits vertical tab, `0x0B`, and so would split
/// `E'11\x0b22'::oidvector` into one element instead of two).
pub(crate) fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

fn needs_quote(s: &str, delim: char) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars()
            .any(|c| c == delim || matches!(c, '{' | '}' | '"' | '\\') || is_c_space(c))
}

fn push_quoted(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

/// `array_in`: parse an array literal into its dimensions and its row-major
/// element list, coercing each element token to `elem` through the shared cast
/// machinery (so an element parses exactly like the same scalar literal). An
/// unquoted, case-insensitive `NULL` token is a NULL element; a quoted `"NULL"`
/// is the text "NULL".
///
/// Dimensionality is read off the brace nesting: every group at a given depth
/// must hold the same number of children, and scalars may only appear at the
/// deepest level, or the literal is malformed. An optional `[lower:upper]…=`
/// prefix fixes the subscript bounds and must agree with the braces that follow.
/// A literal with no elements at all (`{}`, and also `{{}}`) has *no*
/// dimensions.
pub fn array_in(
    input: &str,
    elem: PgType,
    fmt: &FmtCtx,
) -> Result<(Vec<ArrayDim>, Vec<Value>), ArrayError> {
    let delim = elem.typdelim();
    let trimmed = input.trim();
    let mut chars = trimmed.chars().peekable();
    let declared = read_dimension_prefix(&mut chars, input)?;
    if chars.peek() != Some(&'{') {
        return Err(malformed(
            input,
            if declared.is_some() {
                DETAIL_NO_CONTENTS
            } else {
                DETAIL_START
            },
        ));
    }
    let mut scan = Scan {
        input,
        delim,
        elem,
        fmt,
        counts: [None; MAXDIM],
        leaf_level: None,
        elems: Vec::new(),
    };
    scan.group(&mut chars, 0)?;
    skip_ws(&mut chars);
    if chars.next().is_some() {
        return Err(malformed(input, DETAIL_JUNK));
    }
    // No scalar anywhere means an empty array, whatever the brace nesting was.
    let Some(ndims) = scan.leaf_level else {
        // A prefix still has to be self-consistent, but `[l:u]` with a positive
        // length cannot describe an empty literal.
        if declared
            .as_ref()
            .is_some_and(|d| d.iter().any(|d| d.len > 0))
        {
            return Err(malformed(input, DETAIL_DIM_MISMATCH));
        }
        return Ok((Vec::new(), Vec::new()));
    };
    let mut dims: Vec<ArrayDim> = scan.counts[..ndims]
        .iter()
        .map(|len| ArrayDim::new(len.expect("every level above a scalar was scanned")))
        .collect();
    if let Some(declared) = declared {
        if declared.len() != dims.len()
            || declared
                .iter()
                .zip(&dims)
                .any(|(d, actual)| d.len != actual.len)
        {
            return Err(malformed(input, DETAIL_DIM_MISMATCH));
        }
        dims = declared;
    }
    Ok((dims, scan.elems))
}

/// Read an optional `[lower:upper][lower:upper]…=` prefix, leaving the iterator
/// on the first `{`. `None` when the literal does not start with `[`.
///
/// A dimension may also name only its upper bound (`[3]`, meaning `[1:3]`), and
/// the two spellings mix freely (`'[1:2][3]={{1,2,3},{4,5,6}}'`). Whitespace is
/// allowed *between* the brackets and around the `=`, but not *inside* a bracket
/// pair: PostgreSQL accepts `'[2:3] = {1,2}'` and rejects `'[ 2 : 3 ]={1,2}'`.
fn read_dimension_prefix(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    input: &str,
) -> Result<Option<Vec<ArrayDim>>, ArrayError> {
    if chars.peek() != Some(&'[') {
        return Ok(None);
    }
    let mut dims = Vec::new();
    while chars.peek() == Some(&'[') {
        chars.next();
        if dims.len() == MAXDIM {
            return Err(too_many_dimensions());
        }
        // The first bound is the lower one only if a `:` follows it; a bound that
        // runs straight into `]` is the upper one, at the default lower bound.
        let (first, ended_pair) = read_bound(chars, input)?;
        let (lower, upper) = if ended_pair {
            (1, first)
        } else {
            (first, read_bound(chars, input)?.0)
        };
        if upper < lower {
            // Not a malformed literal to PostgreSQL — the syntax parsed, the
            // bounds are what it objects to.
            return Err(ArrayError {
                sqlstate: ARRAY_SUBSCRIPT_ERROR,
                message: "upper bound cannot be less than lower bound".to_string(),
                detail: None,
            });
        }
        // Held in i64 so a `[-2147483648:2147483647]` prefix cannot wrap the
        // length it implies.
        let len = i64::from(upper) - i64::from(lower) + 1;
        let len = i32::try_from(len).map_err(|_| too_many_dimensions())?;
        dims.push(ArrayDim { lower, len });
        skip_ws(chars);
    }
    if chars.next() != Some('=') {
        return Err(malformed(input, DETAIL_NO_EQUALS));
    }
    skip_ws(chars);
    Ok(Some(dims))
}

/// One signed decimal bound inside a `[…]` pair, returning its value and whether
/// the `]` that ends the whole pair came with it (rather than a `:`).
///
/// The digits are taken exactly as written — no trimming — because PostgreSQL
/// treats a space inside the brackets as malformed. A leading `+` or `-` is fine,
/// which `i32`'s own parser already accepts.
fn read_bound(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    input: &str,
) -> Result<(i32, bool), ArrayError> {
    let mut s = String::new();
    let ended_pair = loop {
        match chars.next() {
            Some(']') => break true,
            Some(':') => break false,
            Some(c) => s.push(c),
            None => return Err(malformed(input, DETAIL_NO_RBRACKET)),
        }
    };
    let value = s
        .parse::<i32>()
        .map_err(|_| malformed(input, DETAIL_BAD_DIMS))?;
    Ok((value, ended_pair))
}

/// The `54000` raised when a literal nests more than [`MAXDIM`] brace levels.
/// The parser does not say how deep the literal went — only the constructor
/// does, via [`too_many_dimensions_n`].
fn too_many_dimensions() -> ArrayError {
    ArrayError {
        sqlstate: PROGRAM_LIMIT_EXCEEDED,
        message: format!("number of array dimensions exceeds the maximum allowed ({MAXDIM})"),
        detail: None,
    }
}

/// The same `54000` as [`too_many_dimensions`], worded as the `ARRAY[…]`
/// constructor words it: naming the count it was asked for.
fn too_many_dimensions_n(n: usize) -> ArrayError {
    ArrayError {
        sqlstate: PROGRAM_LIMIT_EXCEEDED,
        message: format!("number of array dimensions ({n}) exceeds the maximum allowed ({MAXDIM})"),
        detail: None,
    }
}

/// The state threaded through the brace scan: `counts[level]` is how many
/// children a group at that level holds, established by the first such group and
/// enforced on every later one, and `leaf_level` is the depth at which scalars
/// were found — which is the array's dimensionality.
struct Scan<'a> {
    input: &'a str,
    delim: char,
    elem: PgType,
    fmt: &'a FmtCtx,
    /// Indexed by level, not by visit order — the scan is depth-first, so a
    /// nested group pins its own level down before its parent does.
    counts: [Option<usize>; MAXDIM],
    leaf_level: Option<usize>,
    elems: Vec<Value>,
}

impl Scan<'_> {
    /// Scan one brace group sitting at `level`; its children are at `level + 1`.
    /// Consumes the opening `{` through the matching `}`.
    fn group(
        &mut self,
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        level: usize,
    ) -> Result<(), ArrayError> {
        if level + 1 > MAXDIM {
            return Err(too_many_dimensions());
        }
        debug_assert_eq!(chars.peek(), Some(&'{'));
        chars.next();
        let mut count = 0usize;
        skip_ws(chars);
        if chars.peek() == Some(&'}') {
            chars.next();
            return self.record(level, 0);
        }
        loop {
            skip_ws(chars);
            if chars.peek() == Some(&'{') {
                // A sub-array where a scalar was already seen at this depth is
                // the ragged case (`{1,{2}}`).
                if self.leaf_level == Some(level + 1) {
                    return Err(malformed(self.input, DETAIL_RAGGED));
                }
                self.group(chars, level + 1)?;
            } else {
                self.scalar(chars, level + 1)?;
            }
            count += 1;
            skip_ws(chars);
            match chars.next() {
                Some(c) if c == self.delim => skip_ws(chars),
                Some('}') => break,
                None => return Err(malformed(self.input, DETAIL_EOF)),
                Some(_) => return Err(malformed(self.input, detail_delim(self.delim))),
            }
        }
        self.record(level, count)
    }

    /// Pin down (or check) how many children a group at `level` holds.
    fn record(&mut self, level: usize, count: usize) -> Result<(), ArrayError> {
        match self.counts[level] {
            None => {
                self.counts[level] = Some(count);
                Ok(())
            }
            Some(established) if established == count => Ok(()),
            Some(_) => Err(malformed(self.input, DETAIL_RAGGED)),
        }
    }

    /// Read one element token at `level` and append its value.
    fn scalar(
        &mut self,
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        level: usize,
    ) -> Result<(), ArrayError> {
        // Scalars must all sit at the same depth; `{{1,2},3}` puts one shallower.
        match self.leaf_level {
            None => self.leaf_level = Some(level),
            Some(l) if l == level => {}
            Some(_) => return Err(malformed(self.input, DETAIL_RAGGED)),
        }
        let (token, quoted) = read_element(chars, self.input, self.delim)?;
        // An empty, unquoted, unescaped token (`{a,,c}`, `{1,}`, `{,1}`) is a
        // missing element, which PG rejects as malformed. A quoted `""` is a
        // legitimate empty-string element and keeps `quoted = true`. PG's DETAIL
        // names the character that follows the missing element.
        if !quoted && token.is_empty() {
            let detail = match chars.peek() {
                Some('}') => DETAIL_RBRACE,
                Some(&c) if c == self.delim => detail_delim(self.delim),
                _ => DETAIL_EOF,
            };
            return Err(malformed(self.input, detail));
        }
        if !quoted && token.eq_ignore_ascii_case("null") {
            self.elems.push(Value::Null);
        } else {
            let v = cast::cast_value(Value::Text(token), self.elem, self.fmt).map_err(|e| {
                ArrayError {
                    sqlstate: e.sqlstate,
                    message: e.message,
                    detail: e.detail,
                }
            })?;
            self.elems.push(v);
        }
        Ok(())
    }
}

fn skip_ws(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while matches!(chars.peek(), Some(&c) if is_c_space(c)) {
        chars.next();
    }
}

/// Read one element token, returning its unescaped text and whether any part was
/// double-quoted or backslash-escaped (which forces it to text and disables the
/// NULL keyword). Leaves the iterator positioned on the following delimiter
/// (`,`/`}`). Trailing **unquoted, unescaped** whitespace is trimmed, but
/// whitespace that was quoted or escaped is significant and kept.
fn read_element(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    input: &str,
    delim: char,
) -> Result<(String, bool), ArrayError> {
    if chars.peek() == Some(&'"') {
        chars.next();
        let mut s = String::new();
        loop {
            match chars.next() {
                Some('\\') => match chars.next() {
                    Some(c) => s.push(c),
                    None => return Err(malformed(input, DETAIL_EOF)),
                },
                Some('"') => return Ok((s, true)),
                Some(c) => s.push(c),
                None => return Err(malformed(input, DETAIL_EOF)),
            }
        }
    }
    // Unquoted: read until a delimiter, honoring backslash escapes and interior
    // quotes. `last_sig` tracks the length up to the last significant (non-
    // whitespace, or quoted/escaped) character, so trailing unquoted whitespace
    // is dropped while an escaped/quoted trailing space is preserved.
    let mut s = String::new();
    let mut forced_text = false;
    let mut last_sig = 0usize;
    loop {
        match chars.peek() {
            Some(&c) if c == delim => break,
            Some('}') | None => break,
            Some('{') => return Err(malformed(input, DETAIL_LBRACE)),
            Some('\\') => {
                chars.next();
                match chars.next() {
                    Some(c) => {
                        s.push(c);
                        forced_text = true;
                        last_sig = s.len();
                    }
                    None => return Err(malformed(input, DETAIL_EOF)),
                }
            }
            Some('"') => {
                chars.next();
                forced_text = true;
                loop {
                    match chars.next() {
                        Some('\\') => match chars.next() {
                            Some(c) => s.push(c),
                            None => return Err(malformed(input, DETAIL_EOF)),
                        },
                        Some('"') => break,
                        Some(c) => s.push(c),
                        None => return Err(malformed(input, DETAIL_EOF)),
                    }
                }
                last_sig = s.len();
            }
            Some(&c) => {
                chars.next();
                s.push(c);
                if !is_c_space(c) {
                    last_sig = s.len();
                }
            }
        }
    }
    s.truncate(last_sig);
    Ok((s, forced_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Just the elements of a literal, for the tests that do not care about its
    /// shape.
    fn parse(input: &str, elem: PgType) -> Result<Vec<Value>, ArrayError> {
        Ok(array_in(input, elem, &FmtCtx::utc_default())?.1)
    }

    fn round_trip(input: &str, elem: PgType) -> Result<String, ArrayError> {
        let (dims, elems) = array_in(input, elem, &FmtCtx::utc_default())?;
        Ok(format(elem, &dims, &elems, &FmtCtx::utc_default()))
    }

    #[test]
    fn round_trips_int_array() -> Result<(), ArrayError> {
        let (dims, elems) = array_in("{1,2,3}", PgType::Int4, &FmtCtx::utc_default())?;
        assert_eq!(elems, vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]);
        assert_eq!(dims, vec![ArrayDim { lower: 1, len: 3 }]);
        assert_eq!(
            format(PgType::Int4, &dims, &elems, &FmtCtx::utc_default()),
            "{1,2,3}"
        );
        Ok(())
    }

    /// An empty array has *no* dimensions, however many brace levels the literal
    /// spells it with — which is why `array_dims('{}'::int[])` is NULL in PG.
    #[test]
    fn empty_array_has_no_dimensions() -> Result<(), ArrayError> {
        for input in ["{}", "{{}}", "{{},{}}"] {
            let (dims, elems) = array_in(input, PgType::Int4, &FmtCtx::utc_default())?;
            assert_eq!(elems, vec![], "for `{input}`");
            assert_eq!(dims, vec![], "for `{input}`");
            assert_eq!(round_trip(input, PgType::Int4)?, "{}", "for `{input}`");
        }
        assert_eq!(format(PgType::Int4, &[], &[], &FmtCtx::utc_default()), "{}");
        Ok(())
    }

    #[test]
    fn round_trips_a_multi_dimensional_array() -> Result<(), ArrayError> {
        let (dims, elems) = array_in("{{1,2,3},{4,5,6}}", PgType::Int4, &FmtCtx::utc_default())?;
        assert_eq!(
            elems,
            (1..=6).map(Value::Int4).collect::<Vec<_>>(),
            "elements are stored row-major"
        );
        assert_eq!(
            dims,
            vec![ArrayDim { lower: 1, len: 2 }, ArrayDim { lower: 1, len: 3 }]
        );
        assert_eq!(
            round_trip("{{1,2,3},{4,5,6}}", PgType::Int4)?,
            "{{1,2,3},{4,5,6}}"
        );
        assert_eq!(
            round_trip("{ { {1,2} , {3,4} } }", PgType::Int4)?,
            "{{{1,2},{3,4}}}"
        );
        Ok(())
    }

    /// A `[lower:upper]=` prefix fixes the subscript bounds. It prints back out
    /// only when some bound is not the default 1.
    #[test]
    fn dimension_prefix_round_trips() -> Result<(), ArrayError> {
        let (dims, elems) = array_in("[2:3]={1,2}", PgType::Int4, &FmtCtx::utc_default())?;
        assert_eq!(elems, vec![Value::Int4(1), Value::Int4(2)]);
        assert_eq!(dims, vec![ArrayDim { lower: 2, len: 2 }]);
        assert_eq!(round_trip("[2:3]={1,2}", PgType::Int4)?, "[2:3]={1,2}");
        assert_eq!(
            round_trip("[2:3][0:1]={{1,2},{3,4}}", PgType::Int4)?,
            "[2:3][0:1]={{1,2},{3,4}}"
        );
        assert_eq!(round_trip("[1:2]={1,2}", PgType::Int4)?, "{1,2}");
        assert_eq!(round_trip("[-2:-1]={1,2}", PgType::Int4)?, "[-2:-1]={1,2}");
        assert_eq!(round_trip("[+2:3]={1,2}", PgType::Int4)?, "[2:3]={1,2}");
        Ok(())
    }

    /// A dimension may name only its upper bound, which puts the lower one at the
    /// default 1, and the two spellings mix within one prefix.
    #[test]
    fn upper_bound_only_dimensions() -> Result<(), ArrayError> {
        let (dims, elems) = array_in("[3]={1,2,3}", PgType::Int4, &FmtCtx::utc_default())?;
        assert_eq!(elems, (1..=3).map(Value::Int4).collect::<Vec<_>>());
        assert_eq!(dims, vec![ArrayDim { lower: 1, len: 3 }]);
        assert_eq!(round_trip("[3]={1,2,3}", PgType::Int4)?, "{1,2,3}");
        assert_eq!(
            round_trip("[1:2][3]={{1,2,3},{4,5,6}}", PgType::Int4)?,
            "{{1,2,3},{4,5,6}}"
        );
        assert_eq!(
            array_in("[0]={1}", PgType::Int4, &FmtCtx::utc_default())
                .expect_err("an upper bound of 0 is below the implied lower bound of 1")
                .sqlstate,
            ARRAY_SUBSCRIPT_ERROR
        );
        Ok(())
    }

    /// Whitespace is allowed between the bracket pairs and around the `=`, but a
    /// bracket pair itself has to hold nothing but its bounds.
    #[test]
    fn whitespace_around_the_dimension_prefix() -> Result<(), ArrayError> {
        for input in [
            "[2:3]={1,2}",
            "[2:3] ={1,2}",
            "[2:3]= {1,2}",
            "[2:3] = {1,2}",
            "  [2:3]={1,2}  ",
        ] {
            assert_eq!(
                round_trip(input, PgType::Int4)?,
                "[2:3]={1,2}",
                "for `{input}`"
            );
        }
        assert_eq!(
            round_trip("[1:2] [1:2]={{1,2},{3,4}}", PgType::Int4)?,
            "{{1,2},{3,4}}"
        );
        let e = array_in("[ 2 : 3 ]={1,2}", PgType::Int4, &FmtCtx::utc_default())
            .expect_err("whitespace inside a bracket pair is malformed");
        assert_eq!(e.sqlstate, INVALID_TEXT_REPRESENTATION);
        assert_eq!(e.detail.as_deref(), Some(DETAIL_BAD_DIMS));
        Ok(())
    }

    /// Each way a dimension prefix can be malformed has its own DETAIL.
    #[test]
    fn dimension_prefix_details_match_pg() {
        let d = |s: &str| {
            array_in(s, PgType::Int4, &FmtCtx::utc_default())
                .expect_err("a malformed dimension prefix must be rejected")
                .detail
                .expect("a malformed-literal error carries a DETAIL line")
        };
        assert_eq!(d("[a:b]={1,2}"), DETAIL_BAD_DIMS);
        assert_eq!(d("[]={1,2}"), DETAIL_BAD_DIMS);
        assert_eq!(d("[2:3"), DETAIL_NO_RBRACKET);
        assert_eq!(d("[1:2]"), DETAIL_NO_EQUALS);
        assert_eq!(d("[1:2]x={1,2}"), DETAIL_NO_EQUALS);
        assert_eq!(d("[1:2] [1:2]"), DETAIL_NO_EQUALS);
        assert_eq!(d("[2:3]="), DETAIL_NO_CONTENTS);
        assert_eq!(d("[2:3]=  "), DETAIL_NO_CONTENTS);
        // A literal that starts with neither `{` nor `[` never reaches the
        // prefix parser at all.
        assert_eq!(d("1,2,3"), DETAIL_START);
    }

    #[test]
    fn ragged_and_mismatched_literals_are_malformed() {
        let d = |s: &str| {
            array_in(s, PgType::Int4, &FmtCtx::utc_default())
                .expect_err("a ragged array literal must be rejected")
        };
        // Sub-arrays of differing length, a scalar where a sub-array belongs,
        // and a sub-array where a scalar belongs.
        for s in ["{{1,2},{3}}", "{{1,2},3}", "{1,{2}}", "{{1},{{2}}}"] {
            let e = d(s);
            assert_eq!(e.sqlstate, INVALID_TEXT_REPRESENTATION, "for `{s}`");
            assert_eq!(e.detail.as_deref(), Some(DETAIL_RAGGED), "for `{s}`");
        }
        // A prefix that disagrees with the braces, in a length or in the number
        // of dimensions.
        for s in ["[1:3]={1,2}", "[2:3][1:2]={1,2}"] {
            assert_eq!(
                d(s).detail.as_deref(),
                Some(DETAIL_DIM_MISMATCH),
                "for `{s}`"
            );
        }
        assert_eq!(d("[1:2]{1,2}").detail.as_deref(), Some(DETAIL_NO_EQUALS));
    }

    /// PostgreSQL caps arrays at `MAXDIM` dimensions, and says so with `54000`
    /// rather than calling the literal malformed.
    #[test]
    fn too_many_dimensions_is_a_program_limit() {
        let e = array_in("{{{{{{{1}}}}}}}", PgType::Int4, &FmtCtx::utc_default())
            .expect_err("seven dimensions must be rejected");
        assert_eq!(e.sqlstate, PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            e.message,
            "number of array dimensions exceeds the maximum allowed (6)"
        );
        assert!(array_in("{{{{{{1}}}}}}", PgType::Int4, &FmtCtx::utc_default()).is_ok());
    }

    /// The `ARRAY[…]` constructor is capped at [`MAXDIM`] too, and words that
    /// limit differently from the literal parser: it names the count it was
    /// asked for.
    #[test]
    fn stacking_past_maxdim_is_a_program_limit() -> Result<(), ArrayError> {
        let mut v = Value::array_1d(PgType::Int4, vec![Value::Int4(1)]);
        for _ in 1..MAXDIM {
            v = stack(PgType::Int4, std::slice::from_ref(&v))?;
        }
        let Value::Array { dims, .. } = &v else {
            unreachable!("stack returns an array");
        };
        assert_eq!(dims.len(), MAXDIM, "six dimensions are still fine");

        let e = stack(PgType::Int4, std::slice::from_ref(&v))
            .expect_err("a seventh dimension must be rejected");
        assert_eq!(e.sqlstate, PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            e.message,
            "number of array dimensions (7) exceeds the maximum allowed (6)"
        );
        Ok(())
    }

    /// Operands that disagree about their shape are a different failure, with
    /// PG's own wording.
    #[test]
    fn stacking_ragged_operands_is_a_subscript_error() {
        let two = Value::array_1d(PgType::Int4, vec![Value::Int4(1), Value::Int4(2)]);
        let one = Value::array_1d(PgType::Int4, vec![Value::Int4(3)]);
        let e = stack(PgType::Int4, &[two.clone(), one])
            .expect_err("sub-arrays of differing length must be rejected");
        assert_eq!(e.sqlstate, ARRAY_SUBSCRIPT_ERROR);
        assert_eq!(
            e.message,
            "multidimensional arrays must have array expressions with matching dimensions"
        );
        assert_eq!(
            stack(PgType::Int4, &[two, Value::Null])
                .expect_err("a NULL operand must be rejected")
                .sqlstate,
            ARRAY_SUBSCRIPT_ERROR
        );
    }

    /// An inverted `[upper:lower]` is not a *syntax* problem, so PG reports it as
    /// an array-subscript error instead of a malformed literal.
    #[test]
    fn inverted_bounds_are_a_subscript_error() {
        let e = array_in("[2:1]={}", PgType::Int4, &FmtCtx::utc_default())
            .expect_err("an inverted bound pair must be rejected");
        assert_eq!(e.sqlstate, ARRAY_SUBSCRIPT_ERROR);
        assert_eq!(e.message, "upper bound cannot be less than lower bound");
        assert_eq!(e.detail, None);
    }

    #[test]
    fn null_and_quoting() -> Result<(), ArrayError> {
        let elems = parse(r#"{a,"b,c",NULL,"NULL",""}"#, PgType::Text)?;
        assert_eq!(
            elems,
            vec![
                Value::Text("a".into()),
                Value::Text("b,c".into()),
                Value::Null,
                Value::Text("NULL".into()),
                Value::Text("".into()),
            ]
        );
        // Round-trip: the delimiter/empty/NULL-lookalike elements are quoted.
        assert_eq!(
            round_trip(r#"{a,"b,c",NULL,"NULL",""}"#, PgType::Text)?,
            r#"{a,"b,c",NULL,"NULL",""}"#
        );
        Ok(())
    }

    #[test]
    fn whitespace_between_elements_is_trimmed() -> Result<(), ArrayError> {
        let elems = parse("{ 1 , 2 , 3 }", PgType::Int4)?;
        assert_eq!(elems, vec![Value::Int4(1), Value::Int4(2), Value::Int4(3)]);
        Ok(())
    }

    #[test]
    fn backslash_escape_in_quotes() -> Result<(), ArrayError> {
        let elems = parse(r#"{"a\"b","c\\d"}"#, PgType::Text)?;
        assert_eq!(
            elems,
            vec![Value::Text("a\"b".into()), Value::Text("c\\d".into())]
        );
        Ok(())
    }

    #[test]
    fn malformed_missing_braces() {
        assert!(array_in("1,2,3", PgType::Int4, &FmtCtx::utc_default()).is_err());
        assert!(array_in("{1,2", PgType::Int4, &FmtCtx::utc_default()).is_err());
    }

    #[test]
    fn malformed_detail_matches_pg() {
        // DETAIL strings verified against PostgreSQL's array_in.
        let d = |s: &str| {
            array_in(s, PgType::Text, &FmtCtx::utc_default())
                .expect_err("malformed array literal must be rejected")
                .detail
                .expect("a malformed-literal error carries a DETAIL line")
        };
        assert_eq!(d("1,2,3"), DETAIL_START);
        assert_eq!(d("abc"), DETAIL_START);
        assert_eq!(d("{1,2"), DETAIL_EOF);
        assert_eq!(d("{1,2}}"), DETAIL_JUNK);
        assert_eq!(d("{1,2} junk"), DETAIL_JUNK);
        assert_eq!(d("{a,,c}"), DETAIL_COMMA);
        assert_eq!(d("{,1}"), DETAIL_COMMA);
        assert_eq!(d("{1,}"), DETAIL_RBRACE);
    }

    #[test]
    fn empty_unquoted_element_is_malformed() -> Result<(), ArrayError> {
        // A missing element between/around commas is malformed, but a quoted
        // empty string is a legitimate element.
        assert!(array_in("{a,,c}", PgType::Text, &FmtCtx::utc_default()).is_err());
        assert!(array_in("{1,}", PgType::Text, &FmtCtx::utc_default()).is_err());
        assert!(array_in("{,1}", PgType::Text, &FmtCtx::utc_default()).is_err());
        assert_eq!(
            parse(r#"{a,"",c}"#, PgType::Text)?,
            vec![
                Value::Text("a".into()),
                Value::Text(String::new()),
                Value::Text("c".into())
            ]
        );
        Ok(())
    }

    #[test]
    fn escaped_trailing_whitespace_is_kept() -> Result<(), ArrayError> {
        // A backslash-escaped trailing space is significant and must survive the
        // unquoted trailing-whitespace trim; an unescaped one is dropped.
        assert_eq!(
            parse("{a\\ }", PgType::Text)?,
            vec![Value::Text("a ".into())]
        );
        assert_eq!(parse("{a }", PgType::Text)?, vec![Value::Text("a".into())]);
        Ok(())
    }

    #[test]
    fn non_ascii_whitespace_element_is_not_quoted() {
        // PG's array_out only treats ASCII whitespace as needing quotes; a
        // non-breaking space (U+00A0) is left bare.
        assert_eq!(
            format_1d(
                PgType::Text,
                &[Value::Text("a\u{00A0}b".into())],
                &FmtCtx::utc_default()
            ),
            "{a\u{00A0}b}"
        );
    }

    #[test]
    fn box_arrays_use_a_semicolon_delimiter() -> Result<(), ArrayError> {
        // `box` is the one built-in with `typdelim = ';'`, because its own
        // output text contains commas.
        let elems = parse("{(1,1),(0,0);(3,3),(2,2)}", PgType::Box)?;
        assert_eq!(
            elems,
            vec![
                Value::Box([1.0, 1.0, 0.0, 0.0]),
                Value::Box([3.0, 3.0, 2.0, 2.0]),
            ]
        );
        // Round-trips unquoted: a comma is no longer the delimiter, so the
        // element text does not need quoting.
        assert_eq!(
            round_trip("{(1,1),(0,0);(3,3),(2,2)}", PgType::Box)?,
            "{(1,1),(0,0);(3,3),(2,2)}"
        );
        Ok(())
    }

    #[test]
    fn oid_mapping_round_trips() {
        assert_eq!(array_oid_for_elem(oid::INT4), Some(oid::INT4_ARRAY));
        assert_eq!(elem_oid_for_array(oid::INT4_ARRAY), Some(oid::INT4));
        assert_eq!(array_oid_for_elem(oid::TEXT), Some(oid::TEXT_ARRAY));
    }
}
