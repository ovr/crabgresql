//! `tsvector`: a sorted list of distinct lexemes, each with an optional sorted
//! list of weighted positions.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the accepted input spellings, the canonical `tsvector_out` text,
//! the sort order, and the SQLSTATE/message of each error — derived from the
//! documentation and from differential probing against a real server, and
//! implemented independently.
//!
//! Representation: [`TsVector`] holds lexemes sorted by byte order with
//! duplicates merged, and each lexeme's positions sorted ascending with
//! duplicates collapsed (keeping the *strongest* weight). Those invariants make
//! structural `PartialEq`/`Hash` coincide with tsvector equality, and let [`cmp`]
//! implement PG's total order without normalizing first.
//!
//! Weights are ranked `D < C < B < A`, stored as `0..=3`. `D` is the default and
//! is not printed, matching `tsvector_out`.

use std::cmp::Ordering;

// SQLSTATEs (kept as literals; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const SYNTAX_ERROR: &str = "42601";
const INVALID_PARAMETER_VALUE: &str = "22023";
const NULL_VALUE_NOT_ALLOWED: &str = "22004";
const ZERO_LENGTH_CHARACTER_STRING: &str = "2200F";
/// `setweight` and `ts_filter` disagree on how they report a bad weight:
/// `setweight(v, 'x')` gives `XX000` / `unrecognized weight: 120` (the raw
/// codepoint), while `ts_filter(v, '{x}')` gives `22023` /
/// `unrecognized weight: "x"`. Both spellings are pinned by `setweight_forms`.
const INTERNAL_ERROR: &str = "XX000";

/// Highest position a lexeme may carry. Larger positions are silently clamped
/// to this value on input, matching PG (`'a:16384'::tsvector` yields `'a':16383`
/// with no error or notice).
pub const MAX_POS: u16 = 16383;

/// Maximum number of positions retained for one lexeme; extras are dropped.
const MAX_POSITIONS_PER_LEXEME: usize = 256;

/// A text-search error, carrying the SQLSTATE and message PG reports. Shared
/// with [`crate::tsquery`].
#[derive(Clone, Debug, PartialEq)]
pub struct TsError {
    pub sqlstate: &'static str,
    pub message: String,
}

impl TsError {
    pub(crate) fn new(sqlstate: &'static str, message: impl Into<String>) -> TsError {
        TsError {
            sqlstate,
            message: message.into(),
        }
    }

    /// `syntax error in tsvector: "…"` (42601) — PG quotes the whole input, not
    /// the offending fragment.
    fn syntax(input: &str) -> TsError {
        TsError::new(
            SYNTAX_ERROR,
            format!("syntax error in tsvector: \"{input}\""),
        )
    }
}

/// One weighted position within a lexeme. `pos` is `1..=MAX_POS`; `weight` is
/// `0..=3` ranking `D < C < B < A`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pos {
    pub pos: u16,
    pub weight: u8,
}

/// One lexeme. `positions` is sorted ascending by `pos` with no duplicate `pos`;
/// empty means "no position information" (a stripped lexeme).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Lexeme {
    pub word: String,
    pub positions: Vec<Pos>,
}

/// A `tsvector`. Invariant: `lexemes` is sorted by `word` byte order, with no
/// duplicate words.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct TsVector {
    pub lexemes: Vec<Lexeme>,
}

/// What this vector owns on the heap: the lexeme list, and per lexeme its word
/// and its position list. Not to be confused with [`payload_size`], which models
/// PostgreSQL's *on-disk* footprint because that is what orders two tsvectors.
pub fn heap_bytes(tv: &TsVector) -> usize {
    crate::footprint::slice_bytes::<Lexeme>(tv.lexemes.capacity())
        + tv
            .lexemes
            .iter()
            .map(|lexeme| {
                crate::footprint::alloc_bytes(lexeme.word.capacity())
                    + crate::footprint::slice_bytes::<Pos>(lexeme.positions.capacity())
            })
            .sum::<usize>()
}

/// The one weight-letter table. `D` is the weakest and `A` the strongest; every
/// other weight lookup in this module is a thin wrapper over this, so the
/// spellings accepted by input, `setweight` and `ts_filter` cannot drift apart.
fn letter_rank(c: char) -> Option<u8> {
    match c {
        'A' | 'a' => Some(3),
        'B' | 'b' => Some(2),
        'C' | 'c' => Some(1),
        'D' | 'd' => Some(0),
        _ => None,
    }
}

/// Map a weight character as it appears *in tsvector input*, where `*` is an
/// accepted spelling of `A` — which is what makes `'a:1*'` parse.
fn weight_rank(c: char) -> Option<u8> {
    if c == '*' {
        return Some(3);
    }
    letter_rank(c)
}

/// The letter `tsvector_out` prints for a weight rank. `D` (the default) prints
/// as nothing.
fn weight_char(rank: u8) -> Option<char> {
    match rank {
        3 => Some('A'),
        2 => Some('B'),
        1 => Some('C'),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// `tsvector_in`: parse the text representation.
///
/// Grammar (derived from observed behavior): whitespace-separated entries, each
/// a lexeme optionally followed by `:` and a comma-separated position list.
/// A lexeme is either bare (running to whitespace or a `:` that introduces
/// positions) or single-quoted, with `''` for an embedded quote; in both forms
/// `\` escapes the next character. A position is decimal digits followed by an
/// optional run of weight letters.
pub fn tsvector_in(input: &str) -> Result<TsVector, TsError> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut lexemes: Vec<Lexeme> = Vec::new();

    loop {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let word = scan_lexeme(&chars, &mut i, &[':'], || TsError::syntax(input))?;
        // An empty lexeme is never storable, whether it was written `''` or
        // produced by an escape run.
        if word.is_empty() {
            return Err(TsError::syntax(input));
        }
        let positions = if i < chars.len() && chars[i] == ':' {
            i += 1;
            scan_positions(&chars, &mut i, input)?
        } else {
            Vec::new()
        };
        // The next entry must be separated by whitespace — except after a
        // quoted lexeme, which is self-delimiting (`'a'b` is two lexemes).
        lexemes.push(Lexeme { word, positions });
    }

    Ok(build(lexemes))
}

/// Read one lexeme starting at `*i`, consuming its quotes and escapes. Shared
/// with [`crate::tsquery`], which spells lexemes identically but ends a bare one
/// at its own operator characters — hence `stops`. Whitespace always ends a bare
/// lexeme; a quoted one is self-delimiting, so `'a'b` is two lexemes.
///
/// `err` builds the caller's "syntax error in <type>" error, so each type
/// reports its own wording for an unterminated quote or a dangling backslash.
pub(crate) fn scan_lexeme(
    chars: &[char],
    i: &mut usize,
    stops: &[char],
    err: impl Fn() -> TsError,
) -> Result<String, TsError> {
    let mut word = String::new();
    if chars.get(*i) == Some(&'\'') {
        *i += 1;
        loop {
            let Some(&c) = chars.get(*i) else {
                // Unterminated quote.
                return Err(err());
            };
            match c {
                '\'' => {
                    // `''` is an escaped quote; a lone `'` closes the lexeme.
                    if chars.get(*i + 1) == Some(&'\'') {
                        word.push('\'');
                        *i += 2;
                    } else {
                        *i += 1;
                        break;
                    }
                }
                '\\' => {
                    let Some(&esc) = chars.get(*i + 1) else {
                        return Err(err());
                    };
                    word.push(esc);
                    *i += 2;
                }
                _ => {
                    word.push(c);
                    *i += 1;
                }
            }
        }
        return Ok(word);
    }

    while let Some(&c) = chars.get(*i) {
        match c {
            '\\' => {
                let Some(&esc) = chars.get(*i + 1) else {
                    return Err(err());
                };
                word.push(esc);
                *i += 2;
            }
            c if c.is_whitespace() || stops.contains(&c) => break,
            _ => {
                word.push(c);
                *i += 1;
            }
        }
    }
    Ok(word)
}

/// Read the comma-separated position list following a `:`.
fn scan_positions(chars: &[char], i: &mut usize, input: &str) -> Result<Vec<Pos>, TsError> {
    let mut out: Vec<Pos> = Vec::new();
    loop {
        let start = *i;
        let mut num: u32 = 0;
        while let Some(c) = chars.get(*i).and_then(|c| c.to_digit(10)) {
            // Saturate rather than overflow; anything this large clamps anyway.
            num = num.saturating_mul(10).saturating_add(c);
            *i += 1;
        }
        if *i == start {
            // `:` with no digits, e.g. `a:` or `a:b`.
            return Err(TsError::syntax(input));
        }
        if num == 0 {
            return Err(TsError::new(
                SYNTAX_ERROR,
                format!("wrong position info in tsvector: \"{input}\""),
            ));
        }
        let pos = num.min(MAX_POS as u32) as u16;

        // A run of weight letters. `D` is the default and does not terminate the
        // run; any other weight sets the rank and must be the last letter — so
        // `1dc` is C but `1cd` is a syntax error.
        let mut weight = 0u8;
        while let Some(rank) = chars.get(*i).copied().and_then(weight_rank) {
            *i += 1;
            if rank != 0 {
                weight = rank;
                break;
            }
        }
        if chars.get(*i).copied().and_then(weight_rank).is_some() {
            return Err(TsError::syntax(input));
        }

        out.push(Pos { pos, weight });

        match chars.get(*i) {
            Some(',') => *i += 1,
            Some(c) if c.is_whitespace() => break,
            None => break,
            // Anything else after a position is a syntax error (`a:1*A`).
            Some(_) => return Err(TsError::syntax(input)),
        }
    }
    Ok(out)
}

/// Impose the [`TsVector`] invariants on a freshly parsed lexeme list: sort by
/// word, merge duplicates, and sort/dedup each position list.
fn build(mut lexemes: Vec<Lexeme>) -> TsVector {
    lexemes.sort_by(|a, b| a.word.as_bytes().cmp(b.word.as_bytes()));

    let mut out: Vec<Lexeme> = Vec::with_capacity(lexemes.len());
    for lex in lexemes {
        match out.last_mut() {
            Some(prev) if prev.word == lex.word => prev.positions.extend(lex.positions),
            _ => out.push(lex),
        }
    }
    for lex in &mut out {
        normalize_positions(&mut lex.positions);
    }
    TsVector { lexemes: out }
}

/// Sort positions ascending and collapse duplicates, keeping the strongest
/// weight (`'a:1B a:1A'` is `'a':1A`). Truncated to [`MAX_POSITIONS_PER_LEXEME`].
fn normalize_positions(positions: &mut Vec<Pos>) {
    positions.sort_by_key(|p| p.pos);
    let mut out: Vec<Pos> = Vec::with_capacity(positions.len());
    for p in positions.iter() {
        match out.last_mut() {
            Some(prev) if prev.pos == p.pos => prev.weight = prev.weight.max(p.weight),
            _ => out.push(*p),
        }
    }
    out.truncate(MAX_POSITIONS_PER_LEXEME);
    *positions = out;
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// `tsvector_out`: the canonical text form. Every lexeme is single-quoted, with
/// `'` doubled and `\` doubled.
pub fn format(tv: &TsVector) -> String {
    let mut out = String::new();
    for (n, lex) in tv.lexemes.iter().enumerate() {
        if n > 0 {
            out.push(' ');
        }
        format_lexeme(&lex.word, &mut out);
        for (k, p) in lex.positions.iter().enumerate() {
            out.push(if k == 0 { ':' } else { ',' });
            out.push_str(&p.pos.to_string());
            if let Some(w) = weight_char(p.weight) {
                out.push(w);
            }
        }
    }
    out
}

/// Append a lexeme in its quoted, escaped output spelling. Shared with
/// [`crate::tsquery`], whose `tsquery_out` quotes lexemes the same way.
pub(crate) fn format_lexeme(word: &str, out: &mut String) {
    out.push('\'');
    for c in word.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('\'');
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// The storage footprint PG's ordering compares first: a fixed 4-byte entry per
/// lexeme, plus a data area holding each lexeme's bytes and, for a lexeme that
/// carries positions, a count and one entry per position.
///
/// The position block is aligned on the *cumulative* data offset, not per
/// lexeme, so a preceding odd-length word can absorb the padding a later one
/// would otherwise need — which is why `'aaaaaaa' 'x'` and `'a' 'b':14B,18A`
/// tie rather than differing by one byte.
fn payload_size(tv: &TsVector) -> usize {
    let mut data = 0usize;
    for lex in &tv.lexemes {
        data += lex.word.len();
        if !lex.positions.is_empty() {
            data = data.next_multiple_of(2) + 2 + 2 * lex.positions.len();
        }
    }
    4 * tv.lexemes.len() + data
}

/// PG's `tsvector` total order: storage footprint, then lexeme count, then
/// lexeme-by-lexeme (word bytes, then positions).
///
/// The last tier is a documented approximation. When two vectors tie on both
/// footprint *and* lexeme count, PG breaks the tie with a raw byte comparison of
/// its packed representation, whose leading bytes are the per-lexeme
/// length/has-positions header rather than the text — so `'hidden'` sorts after
/// `'x':25` even though `h` < `x`. That layout is an implementation detail with
/// no documented contract, so we compare the lexemes and positions themselves.
/// Measured against PostgreSQL 18.4 over 1540 random pairs, 3 ordered
/// differently; all three tie on both earlier tiers.
pub fn cmp(a: &TsVector, b: &TsVector) -> Ordering {
    payload_size(a)
        .cmp(&payload_size(b))
        .then_with(|| a.lexemes.len().cmp(&b.lexemes.len()))
        .then_with(|| {
            for (x, y) in a.lexemes.iter().zip(&b.lexemes) {
                let ord = x
                    .word
                    .as_bytes()
                    .cmp(y.word.as_bytes())
                    .then_with(|| cmp_positions(&x.positions, &y.positions));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        })
}

/// Compare two position lists. PG orders them by position **descending**
/// (`'a:2' < 'a:1'`) and ignores weights entirely, so `'a:1B'` and `'a:1C'` are
/// neither less nor greater than one another — yet `'a:1B' = 'a:1C'` is false.
/// That makes PG's own ordering inconsistent with its equality.
///
/// We reproduce the descending-position order, then break a weight-only tie
/// instead of reporting `Equal`. Equality, `DISTINCT`, `GROUP BY` and the btree
/// index all route through this comparison here, so returning `Equal` for values
/// that are not equal would be far worse than ordering two weight-only variants
/// in an order PG considers arbitrary.
fn cmp_positions(a: &[Pos], b: &[Pos]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let ord = y.pos.cmp(&x.pos).then_with(|| x.weight.cmp(&y.weight));
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// The largest position in the vector, or 0 if it carries no position data.
pub fn max_pos(tv: &TsVector) -> u16 {
    tv.lexemes
        .iter()
        .filter_map(|l| l.positions.last().map(|p| p.pos))
        .max()
        .unwrap_or(0)
}

/// `tsvector || tsvector`: union the lexemes, shifting the right operand's
/// positions past the left operand's highest position.
pub fn concat(a: &TsVector, b: &TsVector) -> TsVector {
    let shift = max_pos(a);
    let mut lexemes = a.lexemes.clone();
    for lex in &b.lexemes {
        let positions = lex
            .positions
            .iter()
            .map(|p| Pos {
                pos: p.pos.saturating_add(shift).min(MAX_POS),
                weight: p.weight,
            })
            .collect();
        lexemes.push(Lexeme {
            word: lex.word.clone(),
            positions,
        });
    }
    build(lexemes)
}

/// `strip(tsvector)`: drop all position and weight information.
pub fn strip(tv: &TsVector) -> TsVector {
    TsVector {
        lexemes: tv
            .lexemes
            .iter()
            .map(|l| Lexeme {
                word: l.word.clone(),
                positions: Vec::new(),
            })
            .collect(),
    }
}

/// `length(tsvector)`: the number of distinct lexemes.
pub fn length(tv: &TsVector) -> i32 {
    tv.lexemes.len() as i32
}

/// Resolve a weight label to its rank, for `setweight`'s `"char"` argument.
/// PG reports the offending *byte* here, not a quoted string.
pub fn weight_from_char(c: char) -> Result<u8, TsError> {
    letter_rank(c)
        .ok_or_else(|| TsError::new(INTERNAL_ERROR, format!("unrecognized weight: {}", c as u32)))
}

/// Resolve a weight label given as text, for `ts_filter`'s `"char"[]` argument.
/// PG quotes the offending label here.
pub fn weight_from_label(s: &str) -> Result<u8, TsError> {
    label_rank(s).ok_or_else(|| unrecognized_weight_text(s))
}

fn unrecognized_weight_text(s: &str) -> TsError {
    TsError::new(
        INVALID_PARAMETER_VALUE,
        format!("unrecognized weight: \"{s}\""),
    )
}

/// A weight given as text must be exactly one weight letter.
fn label_rank(s: &str) -> Option<u8> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => letter_rank(c),
        _ => None,
    }
}

/// `setweight(tsvector, "char")`: stamp every position with `weight`.
pub fn setweight(tv: &TsVector, weight: u8) -> TsVector {
    setweight_impl(tv, weight, None)
}

/// `setweight(tsvector, "char", text[])`: stamp only the listed lexemes. NULL
/// entries in the array are ignored, matching PG.
pub fn setweight_lexemes(tv: &TsVector, weight: u8, words: &[Option<String>]) -> TsVector {
    let wanted: Vec<&str> = words.iter().filter_map(|w| w.as_deref()).collect();
    setweight_impl(tv, weight, Some(&wanted))
}

fn setweight_impl(tv: &TsVector, weight: u8, only: Option<&[&str]>) -> TsVector {
    TsVector {
        lexemes: tv
            .lexemes
            .iter()
            .map(|l| {
                let apply = only.is_none_or(|words| words.contains(&l.word.as_str()));
                Lexeme {
                    word: l.word.clone(),
                    positions: if apply {
                        l.positions
                            .iter()
                            .map(|p| Pos { pos: p.pos, weight })
                            .collect()
                    } else {
                        l.positions.clone()
                    },
                }
            })
            .collect(),
    }
}

/// `ts_delete(tsvector, text)` / `ts_delete(tsvector, text[])`: remove the named
/// lexemes. Matching is exact — `'bas'` does not remove `'base'`. NULL entries
/// in the array form are ignored.
pub fn ts_delete(tv: &TsVector, words: &[Option<String>]) -> TsVector {
    TsVector {
        lexemes: tv
            .lexemes
            .iter()
            .filter(|l| !words.iter().any(|w| w.as_deref() == Some(l.word.as_str())))
            .cloned()
            .collect(),
    }
}

/// `ts_filter(tsvector, "char"[])`: keep only positions whose weight is in
/// `weights`, dropping any lexeme left with none. Lexemes without position data
/// are dropped, since they have no weight to test.
pub fn ts_filter(tv: &TsVector, weights: &[u8]) -> TsVector {
    let mut lexemes = Vec::new();
    for lex in &tv.lexemes {
        let positions: Vec<Pos> = lex
            .positions
            .iter()
            .filter(|p| weights.contains(&p.weight))
            .copied()
            .collect();
        if !positions.is_empty() {
            lexemes.push(Lexeme {
                word: lex.word.clone(),
                positions,
            });
        }
    }
    TsVector { lexemes }
}

/// `tsvector_to_array(tsvector)`: the lexemes as text, in stored order.
pub fn to_array(tv: &TsVector) -> Vec<String> {
    tv.lexemes.iter().map(|l| l.word.clone()).collect()
}

/// `array_to_tsvector(text[])`: build a positionless tsvector, sorting and
/// de-duplicating. NULLs and empty strings are rejected — neither can be a
/// lexeme.
pub fn from_array(words: &[Option<String>]) -> Result<TsVector, TsError> {
    let mut lexemes = Vec::with_capacity(words.len());
    for w in words {
        let Some(w) = w else {
            return Err(TsError::new(
                NULL_VALUE_NOT_ALLOWED,
                "lexeme array may not contain nulls",
            ));
        };
        if w.is_empty() {
            return Err(TsError::new(
                ZERO_LENGTH_CHARACTER_STRING,
                "lexeme array may not contain empty strings",
            ));
        }
        lexemes.push(Lexeme {
            word: w.clone(),
            positions: Vec::new(),
        });
    }
    Ok(build(lexemes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse and re-emit in canonical form.
    fn round(s: &str) -> Result<String, TsError> {
        Ok(format(&v(s)?))
    }

    fn v(s: &str) -> Result<TsVector, TsError> {
        tsvector_in(s)
    }

    #[test]
    fn trims_and_sorts() -> Result<(), TsError> {
        assert_eq!(round(" 1 ")?, "'1'");
        assert_eq!(round("1 2")?, "'1' '2'");
        assert_eq!(round("b a")?, "'a' 'b'");
        assert_eq!(round("")?, "");
        Ok(())
    }

    #[test]
    fn quoted_lexemes_and_escapes() -> Result<(), TsError> {
        assert_eq!(round("'1 2'")?, "'1 2'");
        assert_eq!(round("'1 ''2'")?, "'1 ''2'");
        // A quoted lexeme is self-delimiting: `'1 ''2'3` is two lexemes.
        assert_eq!(round("'1 ''2'3")?, "'1 ''2' '3'");
        // A backslash escapes a quote inside a quoted lexeme, and a leading
        // space sorts a lexeme first.
        assert_eq!(round(r"'1 \'2' ' 3' 4 ")?, "' 3' '1 ''2' '4'");
        Ok(())
    }

    #[test]
    fn backslash_escapes_round_trip() -> Result<(), TsError> {
        // The upstream `tstypes` case: every spelling collapses one escape level,
        // and output re-doubles the backslashes.
        let out = round(r"'\\as' ab\c ab\\c AB\\\c ab\\\\c")?;
        assert_eq!(out, r"'AB\\c' '\\as' 'ab\\\\c' 'ab\\c' 'abc'");
        // ... and re-parsing that output is a fixed point.
        assert_eq!(round(&out)?, out);
        Ok(())
    }

    #[test]
    fn positions_sort_dedup_and_merge() -> Result<(), TsError> {
        assert_eq!(round("a:1,1")?, "'a':1");
        assert_eq!(round("a:2,1")?, "'a':1,2");
        assert_eq!(round("a:1 a:2")?, "'a':1,2");
        // The strongest weight wins for a repeated position.
        assert_eq!(round("a:1B a:1A")?, "'a':1A");
        assert_eq!(round("a:1A a:1B")?, "'a':1A");
        assert_eq!(round("a:1D a:1C")?, "'a':1C");
        // A positionless duplicate does not erase the positions.
        assert_eq!(round("a:1 a")?, "'a':1");
        Ok(())
    }

    #[test]
    fn weight_letters() -> Result<(), TsError> {
        assert_eq!(round("'w':4A,3B,2C,1D,5 a:8")?, "'a':8 'w':1,2C,3B,4A,5");
        // `*` is a spelling of weight A.
        assert_eq!(round("a:1*")?, "'a':1A");
        assert_eq!(round("a:1,2*")?, "'a':1,2A");
        // `D` keeps the default and lets another letter follow; a set weight
        // terminates the run.
        assert_eq!(round("a:1dc")?, "'a':1C");
        assert_eq!(round("a:1DA")?, "'a':1A");
        assert_eq!(round("a:1ddd")?, "'a':1");
        assert_eq!(round("a:1dddA")?, "'a':1A");
        for bad in ["a:1cd", "a:1BA", "a:1AD", "a:1*A", "a:1*d", "a:1dcba"] {
            assert!(tsvector_in(bad).is_err(), "{bad} should not parse");
        }
        Ok(())
    }

    #[test]
    fn position_limits() -> Result<(), TsError> {
        // Out-of-range positions clamp rather than error.
        assert_eq!(round("a:16384")?, "'a':16383");
        assert_eq!(round("a:99999999999")?, "'a':16383");
        // Position 0 is rejected.
        let err = tsvector_in("a:0").expect_err("rejects position 0");
        assert_eq!(err.sqlstate, SYNTAX_ERROR);
        assert_eq!(err.message, "wrong position info in tsvector: \"a:0\"");
        Ok(())
    }

    #[test]
    fn syntax_errors() -> Result<(), TsError> {
        for bad in ["''", "a:", "a:b", "'unterminated"] {
            let err = tsvector_in(bad).expect_err("rejects");
            assert_eq!(err.sqlstate, SYNTAX_ERROR, "{bad}");
            assert_eq!(err.message, format!("syntax error in tsvector: \"{bad}\""));
        }
        // An empty input is a valid empty tsvector, not an error.
        assert_eq!(tsvector_in(""), Ok(TsVector::default()));
        Ok(())
    }

    #[test]
    fn concat_shifts_positions() -> Result<(), TsError> {
        let a = v("a:3A b:2a")?;
        let b = v("ba:1234 a:1B")?;
        assert_eq!(format(&concat(&a, &b)), "'a':3A,4B 'b':2A 'ba':1237");
        // Concatenating an empty vector on either side is a no-op.
        let empty = TsVector::default();
        assert_eq!(format(&concat(&a, &empty)), format(&a));
        assert_eq!(format(&concat(&empty, &a)), format(&a));
        // Overlapping lexemes merge rather than duplicate.
        let l = v("a b")?;
        let r = v("b:1 c")?;
        assert_eq!(format(&concat(&l, &r)), "'a' 'b':1 'c'");
        Ok(())
    }

    #[test]
    fn strip_and_length() -> Result<(), TsError> {
        let tv = v("w:12B w:13* w:12,5,6 a:1,3* a:3 w asd:1dc asd")?;
        assert_eq!(format(&tv), "'a':1,3A 'asd':1C 'w':5,6,12B,13A");
        assert_eq!(format(&strip(&tv)), "'a' 'asd' 'w'");
        assert_eq!(length(&tv), 3);
        assert_eq!(length(&TsVector::default()), 0);
        Ok(())
    }

    #[test]
    fn setweight_forms() -> Result<(), TsError> {
        let tv = v("a:1,3A asd:1C w:5,6,12B,13A zxc:81,222A,567")?;
        assert_eq!(
            format(&setweight(&tv, 1)),
            "'a':1C,3C 'asd':1C 'w':5C,6C,12C,13C 'zxc':81C,222C,567C"
        );
        let only = [Some("a".to_string()), Some("zxc".to_string())];
        assert_eq!(
            format(&setweight_lexemes(&tv, 1, &only)),
            "'a':1C,3C 'asd':1C 'w':5,6,12B,13A 'zxc':81C,222C,567C"
        );
        // NULL entries in the lexeme array are ignored, not errors.
        let with_null = [Some("a".to_string()), None];
        assert_eq!(
            format(&setweight_lexemes(&tv, 1, &with_null)),
            "'a':1C,3C 'asd':1C 'w':5,6,12B,13A 'zxc':81,222A,567"
        );
        assert_eq!(
            weight_from_char('x').expect_err("bad weight").message,
            "unrecognized weight: 120"
        );
        assert_eq!(
            weight_from_label("x").expect_err("bad weight").message,
            "unrecognized weight: \"x\""
        );
        Ok(())
    }

    #[test]
    fn delete_and_filter() -> Result<(), TsError> {
        let tv = v("base:7 hidden:6 rebel:1 spaceship:2,33A,34B,35C,36D strike:3")?;
        // Deletion is exact-match: neither a prefix nor a plural removes it.
        assert_eq!(format(&ts_delete(&tv, &[Some("bas".into())])), format(&tv));
        assert_eq!(
            format(&ts_delete(&tv, &[Some("bases".into())])),
            format(&tv)
        );
        assert_eq!(
            format(&ts_delete(
                &tv,
                &[Some("spaceship".into()), Some("rebel".into())]
            )),
            "'base':7 'hidden':6 'strike':3"
        );
        // NULLs in the array are skipped. Weight `D` is the default and is not
        // printed, so the `36D` written on input comes back out as plain `36`.
        assert_eq!(
            format(&ts_delete(&tv, &[Some("base".into()), None])),
            "'hidden':6 'rebel':1 'spaceship':2,33A,34B,35C,36 'strike':3"
        );

        let weighted = v("base:7A empir:17 evil:15 hidden:6A rebel:1A won:9")?;
        assert_eq!(
            format(&ts_filter(&weighted, &[3])),
            "'base':7A 'hidden':6A 'rebel':1A"
        );
        // Positionless lexemes have no weight, so nothing survives the filter.
        assert_eq!(format(&ts_filter(&strip(&weighted), &[3])), "");
        Ok(())
    }

    #[test]
    fn array_conversions() -> Result<(), TsError> {
        let tv = v("base:7 hidden:6 rebel:1")?;
        assert_eq!(to_array(&tv), vec!["base", "hidden", "rebel"]);
        // Sorting and de-duplication happen on the way in.
        let built = from_array(&[
            Some("foo".into()),
            Some("bar".into()),
            Some("baz".into()),
            Some("bar".into()),
        ])?;
        assert_eq!(format(&built), "'bar' 'baz' 'foo'");
        assert_eq!(
            from_array(&[Some("a".into()), None])
                .expect_err("rejects null")
                .message,
            "lexeme array may not contain nulls"
        );
        assert_eq!(
            from_array(&[Some("a".into()), Some(String::new())])
                .expect_err("rejects empty")
                .message,
            "lexeme array may not contain empty strings"
        );
        Ok(())
    }

    #[test]
    fn total_order() -> Result<(), TsError> {
        // Storage footprint dominates, ahead of the lexeme count: one long
        // lexeme (11) sorts after two short ones (10).
        assert_eq!(cmp(&v("a")?, &v("a b")?), Ordering::Less);
        assert_eq!(cmp(&v("aaaaaaa")?, &v("a b")?), Ordering::Greater);
        assert_eq!(cmp(&v("zz")?, &v("a:1")?), Ordering::Less);
        // Equal footprint falls to the lexeme count, then to byte order.
        assert_eq!(cmp(&v("a:1")?, &v("a b")?), Ordering::Less);
        assert_eq!(cmp(&v("aa:1")?, &v("b:1")?), Ordering::Less);
        // Positionless lexemes compare by their bytes alone.
        assert_eq!(cmp(&v("b")?, &v("aa")?), Ordering::Less);
        assert_eq!(cmp(&v("a")?, &v("a")?), Ordering::Equal);
        // Positions order descending, matching PG (`'a:2' < 'a:1'`), and still
        // participate in equality.
        assert_eq!(cmp(&v("a:2")?, &v("a:1")?), Ordering::Less);
        assert_eq!(cmp(&v("a:3,4")?, &v("a:1,2")?), Ordering::Less);
        assert_ne!(cmp(&v("a:1")?, &v("a:2")?), Ordering::Equal);
        // A weight-only difference is a tie for PG but must not be `Equal` here,
        // since equality shares this comparison.
        assert_ne!(cmp(&v("a:1B")?, &v("a:1C")?), Ordering::Equal);
        assert_ne!(v("a:1"), v("a"));
        Ok(())
    }
}
