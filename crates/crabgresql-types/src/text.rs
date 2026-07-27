//! String / character type operations.
//!
//! Clean-room (see AGENTS.md): every function reproduces PostgreSQL's
//! *observable* result — the returned value and the SQLSTATE/message of any
//! error — as observed from a running server (PG 18), never ported from PG's C
//! source. Indexing is 1-based and character-based (not byte-based), matching
//! `text`'s behavior in a UTF-8 database.

/// SQLSTATE codes emitted by string functions. Kept local (like `interval.rs`)
/// so `crabgresql-types` needs no dependency on the wire-protocol crate; the
/// executor maps these onto `crabgresql_pg_wire::sqlstate::*`.
mod sqlstate {
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    pub const STRING_DATA_RIGHT_TRUNCATION: &str = "22001";
    pub const NULL_VALUE_NOT_ALLOWED: &str = "22004";
    pub const SUBSTRING_ERROR: &str = "22011";
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    pub const INVALID_ESCAPE_SEQUENCE: &str = "22025";
    pub const INVALID_REGULAR_EXPRESSION: &str = "2201B";
    pub const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
}

/// An error raised by a string function, carrying PG's SQLSTATE and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextError {
    pub sqlstate: &'static str,
    pub message: String,
}

impl TextError {
    fn new(sqlstate: &'static str, message: impl Into<String>) -> Self {
        TextError {
            sqlstate,
            message: message.into(),
        }
    }
}

type Result<T> = std::result::Result<T, TextError>;

/// PostgreSQL's `MaxAllocSize` (1 GB − 1): the largest a single value may grow
/// before `repeat`/`lpad`/`rpad` reject it, matching PG's `requested length too
/// large` error instead of attempting a multi-gigabyte allocation.
const MAX_ALLOC: i64 = 0x3FFF_FFFF;

fn too_large() -> TextError {
    TextError::new(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "requested length too large",
    )
}

// --- length ----------------------------------------------------------------

/// `length` / `char_length` / `character_length`: number of characters.
pub fn char_length(s: &str) -> i32 {
    s.chars().count() as i32
}

/// `octet_length`: number of bytes.
pub fn octet_length(s: &str) -> i32 {
    s.len() as i32
}

/// `bit_length`: number of bits (`octet_length * 8`).
pub fn bit_length(s: &str) -> i32 {
    (s.len() as i32).wrapping_mul(8)
}

// --- case ------------------------------------------------------------------

/// `upper`: full Unicode uppercase mapping.
pub fn upper(s: &str) -> String {
    s.to_uppercase()
}

/// `lower`: full Unicode lowercase mapping.
pub fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// `initcap`: uppercase the first letter of each word, lowercase the rest. A
/// word is a run of alphanumeric characters separated by non-alphanumerics.
pub fn initcap(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_alnum = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if prev_alnum {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_alnum = true;
        } else {
            out.push(c);
            prev_alnum = false;
        }
    }
    out
}

// --- substring / position --------------------------------------------------

/// `substr` / `substring`. `start` is 1-based; `len`, when present, must be
/// non-negative (else SQLSTATE 22011). Positions are clamped to the string.
pub fn substr(s: &str, start: i32, len: Option<i32>) -> Result<String> {
    if let Some(l) = len
        && l < 0
    {
        return Err(TextError::new(
            sqlstate::SUBSTRING_ERROR,
            "negative substring length not allowed",
        ));
    }
    let chars: Vec<char> = s.chars().collect();
    let total = chars.len() as i64;
    let start = start as i64;
    // Exclusive 1-based upper bound: keep positions p with start <= p < end.
    let end = match len {
        Some(l) => start.saturating_add(l as i64),
        None => total + 1,
    };
    let lo = start.max(1);
    let hi = (end - 1).min(total); // inclusive
    if lo > hi {
        return Ok(String::new());
    }
    Ok(chars[(lo - 1) as usize..=(hi - 1) as usize]
        .iter()
        .collect())
}

/// `strpos(string, substring)` / `position(substring IN string)`: the 1-based
/// character index of the first occurrence, 0 if absent. An empty needle → 1.
pub fn strpos(haystack: &str, needle: &str) -> i32 {
    if needle.is_empty() {
        return 1;
    }
    match haystack.find(needle) {
        Some(byte_idx) => haystack[..byte_idx].chars().count() as i32 + 1,
        None => 0,
    }
}

/// `overlay(string placing replacement from start [for count])`. `count`
/// defaults to the replacement's character length. Equivalent to
/// `substr(s,1,start-1) || replacement || substr(s, start+count)`.
pub fn overlay(s: &str, replacement: &str, start: i32, count: Option<i32>) -> Result<String> {
    let count = count.unwrap_or_else(|| char_length(replacement));
    // `substr(s, 1, start-1)` with `start < 1` raises PG's negative-length error,
    // so `start <= 0` is rejected exactly as PG's `text_overlay` does. A negative
    // `start` maps to a negative length without risking i32 overflow.
    let left_len = if start < 1 { -1 } else { start - 1 };
    let left = substr(s, 1, Some(left_len))?;
    let right = substr(s, start.saturating_add(count).max(1), None)?;
    Ok(format!("{left}{replacement}{right}"))
}

// --- trim / pad ------------------------------------------------------------

/// The side(s) to trim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimSide {
    Leading,
    Trailing,
    Both,
}

/// `ltrim` / `rtrim` / `btrim`: remove leading/trailing characters that appear
/// in `chars` (default a single space).
pub fn trim(s: &str, chars: &str, side: TrimSide) -> String {
    let in_set = |c: char| chars.contains(c);
    match side {
        TrimSide::Leading => s.trim_start_matches(in_set).to_string(),
        TrimSide::Trailing => s.trim_end_matches(in_set).to_string(),
        TrimSide::Both => s
            .trim_start_matches(in_set)
            .trim_end_matches(in_set)
            .to_string(),
    }
}

/// `lpad` / `rpad`. When `len <= 0` the result is empty; when the string is
/// longer than `len` it is truncated to the first `len` characters; otherwise
/// it is padded with `fill` (an empty `fill` cannot pad, so the string is
/// returned unchanged).
pub fn pad(s: &str, len: i32, fill: &str, left: bool) -> Result<String> {
    if len <= 0 {
        return Ok(String::new());
    }
    let len = len as usize;
    let s_chars: Vec<char> = s.chars().collect();
    if s_chars.len() >= len {
        return Ok(s_chars[..len].iter().collect());
    }
    let need = len - s_chars.len();
    let fill_chars: Vec<char> = fill.chars().collect();
    if fill_chars.is_empty() {
        return Ok(s.to_string());
    }
    // Reject before allocating: compute the padding's byte length from the fill
    // cycle rather than materializing a multi-gigabyte string (PG's MaxAllocSize).
    let per_cycle_chars = fill_chars.len() as i64;
    let full_cycles = need as i64 / per_cycle_chars;
    let rem = (need as i64 % per_cycle_chars) as usize;
    let rem_bytes: i64 = fill_chars[..rem].iter().map(|c| c.len_utf8() as i64).sum();
    let padding_bytes = full_cycles * fill.len() as i64 + rem_bytes;
    if s.len() as i64 + padding_bytes > MAX_ALLOC {
        return Err(too_large());
    }
    let padding: String = fill_chars.iter().cycle().take(need).collect();
    Ok(if left {
        format!("{padding}{s}")
    } else {
        format!("{s}{padding}")
    })
}

// --- replace / translate / repeat / reverse --------------------------------

/// `replace(string, from, to)`: replace every non-overlapping occurrence. An
/// empty `from` leaves the string unchanged (unlike a naive replace).
pub fn replace(s: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return s.to_string();
    }
    s.replace(from, to)
}

/// `translate(string, from, to)`: replace each character present in `from` with
/// the character at the same index in `to`; characters past the end of `to` are
/// deleted.
pub fn translate(s: &str, from: &str, to: &str) -> String {
    let from_chars: Vec<char> = from.chars().collect();
    let to_chars: Vec<char> = to.chars().collect();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match from_chars.iter().position(|&x| x == c) {
            Some(i) if i < to_chars.len() => out.push(to_chars[i]),
            Some(_) => {} // no replacement char: delete
            None => out.push(c),
        }
    }
    out
}

/// `repeat(string, n)`: `n` copies, empty when `n <= 0`. Rejects a result larger
/// than PG's `MaxAllocSize` instead of attempting the allocation.
pub fn repeat(s: &str, n: i32) -> Result<String> {
    if n <= 0 {
        return Ok(String::new());
    }
    if s.len() as i64 * n as i64 > MAX_ALLOC {
        return Err(too_large());
    }
    Ok(s.repeat(n as usize))
}

/// `reverse`: reverse the characters.
pub fn reverse(s: &str) -> String {
    s.chars().rev().collect()
}

/// `left(string, n)`: the first `n` characters; a negative `n` returns all but
/// the last `|n|`.
pub fn left(s: &str, n: i32) -> String {
    let chars: Vec<char> = s.chars().collect();
    let total = chars.len() as i64;
    let take = if n >= 0 {
        (n as i64).min(total)
    } else {
        (total + n as i64).max(0)
    };
    chars[..take as usize].iter().collect()
}

/// `right(string, n)`: the last `n` characters; a negative `n` returns all but
/// the first `|n|`.
pub fn right(s: &str, n: i32) -> String {
    let chars: Vec<char> = s.chars().collect();
    let total = chars.len() as i64;
    let skip = if n >= 0 {
        (total - n as i64).max(0)
    } else {
        (-(n as i64)).min(total)
    };
    chars[skip as usize..].iter().collect()
}

// --- character codes -------------------------------------------------------

/// `ascii`: the code point of the first character, 0 for an empty string.
pub fn ascii(s: &str) -> i32 {
    s.chars().next().map(|c| c as i32).unwrap_or(0)
}

/// `chr(n)`: the one-character string for code point `n` (UTF-8). `n == 0` and
/// out-of-range values raise PG's errors.
pub fn chr(n: i32) -> Result<String> {
    if n == 0 {
        return Err(TextError::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "null character not permitted",
        ));
    }
    if n < 0 {
        return Err(TextError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "character number must be positive",
        ));
    }
    // Above the Unicode maximum is "too large"; a code point inside the range
    // that is still not a scalar value (a UTF-16 surrogate) is "not valid" — the
    // two distinct PG messages.
    if n as u32 > 0x10FFFF {
        return Err(TextError::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            format!("requested character too large for encoding: {n}"),
        ));
    }
    match char::from_u32(n as u32) {
        Some(c) => Ok(c.to_string()),
        None => Err(TextError::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            format!("requested character not valid for encoding: {n}"),
        )),
    }
}

/// `split_part(string, delimiter, n)`: the `n`-th field (1-based; negative
/// counts from the end). `n == 0` is an error; out-of-range yields the empty
/// string. An empty delimiter treats the whole string as one field.
pub fn split_part(s: &str, delim: &str, n: i32) -> Result<String> {
    if n == 0 {
        return Err(TextError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "field position must not be zero",
        ));
    }
    let parts: Vec<&str> = if delim.is_empty() {
        vec![s]
    } else {
        s.split(delim).collect()
    };
    let idx: Option<usize> = if n > 0 {
        Some((n - 1) as usize)
    } else {
        let from_end = (-n) as usize;
        parts.len().checked_sub(from_end)
    };
    Ok(idx
        .and_then(|i| parts.get(i))
        .map(|x| x.to_string())
        .unwrap_or_default())
}

/// `starts_with(string, prefix)`.
pub fn starts_with(s: &str, prefix: &str) -> bool {
    s.starts_with(prefix)
}

/// `to_hex(int4)`: hexadecimal (unsigned two's-complement) representation.
pub fn to_hex_i32(n: i32) -> String {
    format!("{:x}", n as u32)
}

/// `to_hex(int8)`.
pub fn to_hex_i64(n: i64) -> String {
    format!("{:x}", n as u64)
}

// --- LIKE / ILIKE ----------------------------------------------------------

enum LikeTok {
    Any,       // %
    One,       // _
    Lit(char), // an ordinary (or escaped) literal character
}

/// Parse a LIKE pattern into tokens, honoring `escape` (which makes the next
/// character a literal). A pattern ending in a bare escape character is an
/// error, as in PG.
fn parse_like(pattern: &str, escape: Option<char>) -> Result<Vec<LikeTok>> {
    let mut toks = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            match chars.next() {
                Some(next) => toks.push(LikeTok::Lit(next)),
                None => {
                    return Err(TextError::new(
                        sqlstate::INVALID_ESCAPE_SEQUENCE,
                        "LIKE pattern must not end with escape character",
                    ));
                }
            }
        } else if c == '%' {
            toks.push(LikeTok::Any);
        } else if c == '_' {
            toks.push(LikeTok::One);
        } else {
            toks.push(LikeTok::Lit(c));
        }
    }
    Ok(toks)
}

fn match_tokens(text: &[char], toks: &[LikeTok]) -> bool {
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_ti = 0usize;
    while ti < text.len() {
        let matched = match toks.get(pi) {
            Some(LikeTok::One) => true,
            Some(LikeTok::Lit(c)) => *c == text[ti],
            _ => false,
        };
        if matched {
            pi += 1;
            ti += 1;
        } else if matches!(toks.get(pi), Some(LikeTok::Any)) {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while matches!(toks.get(pi), Some(LikeTok::Any)) {
        pi += 1;
    }
    pi == toks.len()
}

/// `LIKE` (and `ILIKE` when `case_insensitive`). `escape` defaults to `\` at
/// the call site; pass `None` here to disable escaping (`ESCAPE ''`).
pub fn like(s: &str, pattern: &str, escape: Option<char>, case_insensitive: bool) -> Result<bool> {
    if case_insensitive {
        let s = s.to_lowercase();
        let pattern = pattern.to_lowercase();
        let escape = escape.and_then(|e| e.to_lowercase().next());
        let toks = parse_like(&pattern, escape)?;
        let text: Vec<char> = s.chars().collect();
        Ok(match_tokens(&text, &toks))
    } else {
        let toks = parse_like(pattern, escape)?;
        let text: Vec<char> = s.chars().collect();
        Ok(match_tokens(&text, &toks))
    }
}

// --- regex (`~` / `~*`) and SIMILAR TO -------------------------------------

/// Turn a `regex` crate compile failure into PG's `invalid regular expression`
/// error (SQLSTATE `2201B`). PG's detail text differs from the `regex` crate's,
/// but the SQLSTATE and message prefix match observed PG behavior.
fn invalid_regex(e: regex::Error) -> TextError {
    // The crate's `Display` is multi-line; collapse to a single line so the
    // message reads like PG's one-line `invalid regular expression: ...`.
    let detail = e.to_string().split('\n').collect::<Vec<_>>().join(" ");
    TextError::new(
        sqlstate::INVALID_REGULAR_EXPRESSION,
        format!("invalid regular expression: {detail}"),
    )
}

/// The compile-time options a PG regex flags string resolves to.
///
/// The defaults are PG's, *not* the `regex` crate's: an unadorned PG regex is
/// "newline-insensitive", meaning `.` matches a newline and `^`/`$` anchor only
/// at the ends of the string. The crate defaults to the opposite `.`, so
/// `dot_all` starts out `true`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ReOpts {
    case_insensitive: bool,
    multi_line: bool,
    dot_all: bool,
    ignore_whitespace: bool,
    /// The `q` flag: the pattern is a literal string, not a regex.
    literal: bool,
}

impl Default for ReOpts {
    fn default() -> Self {
        ReOpts {
            case_insensitive: false,
            multi_line: false,
            dot_all: true,
            ignore_whitespace: false,
            literal: false,
        }
    }
}

/// Parse a PG regex flags string into compile options plus the `g` ("global")
/// flag, which is not a compile option but a per-function behavior switch.
///
/// Later flags override earlier ones, so `ig` and `ci` behave as in PG. An
/// unrecognized flag is `22023`.
fn parse_re_flags(flags: &str) -> Result<(ReOpts, bool)> {
    let mut opts = ReOpts::default();
    let mut global = false;
    for c in flags.chars() {
        match c {
            'g' => global = true,
            'i' => opts.case_insensitive = true,
            'c' => opts.case_insensitive = false,
            'x' => opts.ignore_whitespace = true,
            // PG's four newline-sensitivity modes. `s` is the default; `n`/`m`
            // make both `.` and the anchors newline-aware, while `p` and `w`
            // each flip only one of the two.
            's' => {
                opts.multi_line = false;
                opts.dot_all = true;
            }
            'n' | 'm' => {
                opts.multi_line = true;
                opts.dot_all = false;
            }
            'p' => {
                opts.multi_line = false;
                opts.dot_all = false;
            }
            'w' => {
                opts.multi_line = true;
                opts.dot_all = true;
            }
            'q' => opts.literal = true,
            // `e` selects a POSIX ERE, which is close enough to the `regex`
            // crate's grammar to accept as-is; `b` selects a BRE, a genuinely
            // different grammar we do not translate.
            'e' => opts.literal = false,
            'b' => {
                return Err(TextError::new(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "basic regular expressions (flag \"b\") are not supported",
                ));
            }
            other => {
                return Err(TextError::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!("invalid regular expression option: \"{other}\""),
                ));
            }
        }
    }
    Ok((opts, global))
}

/// Rewrite `pattern` for the two PG behaviors the `regex` crate cannot express
/// through builder options. Both need to know where bracket expressions start
/// and end, so one walk does both:
///
///   * expanded mode (`x`): whitespace and `#` comments are ignored, but *not*
///     inside a bracket expression, where PG keeps them significant. The
///     crate's `ignore_whitespace` strips them everywhere, so we strip them
///     ourselves and leave that option off.
///   * newline-sensitive modes (`n`/`m`/`p`): a negated bracket expression must
///     not match a newline. The crate's `dot_matches_new_line` covers only `.`,
///     so each `[^...]` is intersected with `[^\n]` using the crate's character
///     class intersection. Wrapping the class whole is what makes this safe —
///     injecting `\n` into it would turn a leading or trailing `-` into a range.
fn rewrite_pattern(pattern: &str, opts: ReOpts) -> String {
    let expand = opts.ignore_whitespace;
    let no_newline = !opts.dot_all;
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // An escape always carries its next character through untouched, so
            // `\ ` stays a literal space even in expanded mode.
            '\\' => {
                out.push('\\');
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '[' => {
                let class = take_bracket(&mut chars);
                // `[^` is negated; `[` alone is not.
                if no_newline && class.starts_with('^') {
                    // `class` already carries its own leading `^`.
                    out.push_str("[[");
                    out.push_str(&class);
                    out.push_str("]&&[^\\n]]");
                } else {
                    out.push('[');
                    out.push_str(&class);
                    out.push(']');
                }
            }
            '#' if expand => {
                // A comment runs to the end of the line.
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if expand && c.is_whitespace() => {}
            other => out.push(other),
        }
    }
    out
}

/// Consume a bracket expression's body from `chars`, which is positioned just
/// after the opening `[`, and return it without the enclosing brackets. Handles
/// the POSIX rules that make `]` a literal member: a leading `^` and a `]` in
/// first position, plus `[:name:]`/`[.x.]`/`[=x=]` sub-expressions. An
/// unterminated class is returned as-is so the regex compiler reports it.
fn take_bracket(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut body = String::new();
    if chars.peek() == Some(&'^') {
        body.push('^');
        chars.next();
    }
    if chars.peek() == Some(&']') {
        body.push(']');
        chars.next();
    }
    while let Some(c) = chars.next() {
        match c {
            ']' => return body,
            // `[:alpha:]` and friends: copy through to the matching delimiter.
            '[' if matches!(chars.peek(), Some(':' | '.' | '=')) => {
                let kind = chars.next().unwrap_or(':');
                body.push('[');
                body.push(kind);
                while let Some(c) = chars.next() {
                    body.push(c);
                    if c == kind && chars.peek() == Some(&']') {
                        body.push(']');
                        chars.next();
                        break;
                    }
                }
            }
            // An escape inside a class still carries its next character.
            '\\' => {
                body.push('\\');
                if let Some(next) = chars.next() {
                    body.push(next);
                }
            }
            other => body.push(other),
        }
    }
    body
}

/// How the cached pattern text turns into a regex. Caching on the *user's*
/// pattern rather than on the compiled source keeps `SIMILAR TO`'s translation
/// out of the per-row path too, not just the compile.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PatternKind {
    /// A POSIX regex, compiled under these options.
    Regex(ReOpts),
    /// A `SIMILAR TO` pattern, translated first (see [`similar_to_regex`]).
    SimilarTo(Option<char>),
}

/// How many compiled patterns to keep per thread. The bound exists to cap
/// per-thread memory for a cache consulted once per row; the exact depth is not
/// observable, so any small number would do.
const RE_CACHE_MAX: usize = 32;

thread_local! {
    /// Most-recently-used first. Cloning a `Regex` is *not* free — it allocates
    /// a fresh, empty match-state pool — so entries are lent out by reference
    /// (see [`with_cached`]) rather than cloned per row.
    static RE_CACHE: std::cell::RefCell<Vec<(String, PatternKind, regex::Regex)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Compile `pattern` according to `kind` (reusing a cached `Regex` when one is
/// live) and run `f` against it.
///
/// `f` must not itself call back into the cache: the entry is lent out while
/// the thread-local is borrowed, so re-entering would panic. Every caller in
/// this module runs a single match and returns an owned result.
fn with_cached<T>(
    pattern: &str,
    kind: PatternKind,
    f: impl FnOnce(&regex::Regex) -> T,
) -> Result<T> {
    RE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        match cache
            .iter()
            .position(|(p, k, _)| *k == kind && p == pattern)
        {
            Some(idx) => {
                // Promote to most-recently-used. Already-hot patterns (the
                // common case for a per-row scan) need no shuffling at all.
                if idx != 0 {
                    cache[..=idx].rotate_right(1);
                }
            }
            None => {
                // The `q` flag makes the whole pattern a literal string;
                // otherwise apply the rewrites the crate cannot express.
                let opts = match kind {
                    PatternKind::Regex(opts) => opts,
                    PatternKind::SimilarTo(_) => ReOpts::default(),
                };
                let source = match kind {
                    PatternKind::SimilarTo(escape) => similar_to_regex(pattern, escape)?,
                    PatternKind::Regex(opts) if opts.literal => regex::escape(pattern),
                    PatternKind::Regex(opts) => rewrite_pattern(pattern, opts),
                };
                let re = regex::RegexBuilder::new(&source)
                    .case_insensitive(opts.case_insensitive)
                    .multi_line(opts.multi_line)
                    .dot_matches_new_line(opts.dot_all)
                    .build()
                    .map_err(invalid_regex)?;
                cache.insert(0, (pattern.to_string(), kind, re));
                cache.truncate(RE_CACHE_MAX);
            }
        }
        Ok(f(&cache[0].2))
    })
}

/// POSIX regex match, backing the `~` (case-sensitive) and `~*`
/// (case-insensitive) operators. The match is *unanchored*: `~` succeeds when
/// the pattern matches anywhere in `s`, as in PG.
pub fn regex_match(s: &str, pattern: &str, case_insensitive: bool) -> Result<bool> {
    let opts = ReOpts {
        case_insensitive,
        ..ReOpts::default()
    };
    with_cached(pattern, PatternKind::Regex(opts), |re| re.is_match(s))
}

/// jsonpath's `like_regex`, which is XQuery-flavored rather than POSIX: `.`
/// does *not* match a newline unless the `s` flag asks for it, the opposite of
/// [`regex_match`]'s default. Only `i s m x q` are meaningful here; PG's
/// jsonpath parser rejects anything else before we see it.
pub fn like_regex_match(s: &str, pattern: &str, flags: &str) -> Result<bool> {
    let opts = ReOpts {
        case_insensitive: flags.contains('i'),
        multi_line: flags.contains('m'),
        dot_all: flags.contains('s'),
        ignore_whitespace: flags.contains('x'),
        literal: flags.contains('q'),
    };
    with_cached(pattern, PatternKind::Regex(opts), |re| re.is_match(s))
}

/// `SIMILAR TO`: an SQL-standard pattern language distinct from both LIKE and
/// POSIX regex. It is case-sensitive and matches the *whole* string (unlike
/// `~`). We translate it to a POSIX regex and delegate to the `regex` crate.
pub fn similar_to_match(s: &str, pattern: &str, escape: Option<char>) -> Result<bool> {
    // The cache is keyed on the SIMILAR TO pattern itself, so a repeated row
    // skips the translation as well as the compile.
    with_cached(pattern, PatternKind::SimilarTo(escape), |re| re.is_match(s))
}

// --- regexp_* functions ----------------------------------------------------

/// Reject the `g` flag for the functions that match at most once. PG raises
/// this *before* compiling the pattern, so an invalid pattern combined with `g`
/// still reports the `g` problem.
fn reject_global(func: &str) -> TextError {
    TextError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("{func}() does not support the \"global\" option"),
    )
}

fn invalid_parameter(name: &str, value: i32) -> TextError {
    TextError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"{name}\": {value}"),
    )
}

/// Translate a 1-based *character* `start` into a byte offset. `Ok(None)` means
/// `start` lies past the end of the string, where PG simply finds no match.
fn start_offset(s: &str, start: i32) -> Result<Option<usize>> {
    if start < 1 {
        return Err(invalid_parameter("start", start));
    }
    // `start` may legitimately point one past the last character, so extend the
    // offsets with the end of the string.
    let mut offsets = s
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(s.len()));
    Ok(offsets.nth(start as usize - 1))
}

/// Rewrite a PG replacement string into the `regex` crate's `$`-based syntax.
///
/// PG recognizes `\1`..`\9` (capture group), `\&` (the whole match) and `\\`
/// (a literal backslash). A backslash before anything else is *not* an error:
/// PG emits both characters literally, so `\q` stays `\q`. A group reference
/// with no corresponding group expands to the empty string, which is also what
/// the `regex` crate does.
fn translate_replacement(replacement: &str) -> String {
    let mut out = String::with_capacity(replacement.len());
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // `$` is a metacharacter for the crate but a literal for PG.
            '$' => out.push_str("$$"),
            '\\' => match chars.peek() {
                // Braced so that `\1x` stays group 1 rather than group `1x`.
                // Only one digit is consumed: PG reads `\10` as group 1 then
                // a literal `0`.
                Some(&d @ '1'..='9') => {
                    chars.next();
                    out.push_str("${");
                    out.push(d);
                    out.push('}');
                }
                Some('&') => {
                    chars.next();
                    out.push_str("${0}");
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                // Any other escape (and a trailing lone backslash) is literal;
                // leave the following character for the next iteration so it
                // still gets its own escaping.
                _ => out.push('\\'),
            },
            other => out.push(other),
        }
    }
    out
}

/// `regexp_replace(source, pattern, replacement [, flags])`. Without the `g`
/// flag only the first match is replaced.
pub fn regexp_replace(s: &str, pattern: &str, replacement: &str, flags: &str) -> Result<String> {
    let (opts, global) = parse_re_flags(flags)?;
    let replacement = translate_replacement(replacement);
    with_cached(pattern, PatternKind::Regex(opts), |re| {
        let out = if global {
            re.replace_all(s, replacement.as_str())
        } else {
            re.replace(s, replacement.as_str())
        };
        out.into_owned()
    })
}

/// `regexp_like(string, pattern [, flags])` — the functional spelling of `~`.
pub fn regexp_like(s: &str, pattern: &str, flags: &str) -> Result<bool> {
    let (opts, global) = parse_re_flags(flags)?;
    if global {
        return Err(reject_global("regexp_like"));
    }
    with_cached(pattern, PatternKind::Regex(opts), |re| re.is_match(s))
}

/// Byte offset one match past `from`, advancing over an empty match by a whole
/// character so the scan cannot stall.
fn advance(s: &str, m: &regex::Match<'_>, from: usize) -> usize {
    if m.end() > from {
        return m.end();
    }
    s[from..]
        .chars()
        .next()
        .map_or(from + 1, |c| from + c.len_utf8())
}

/// `regexp_count(string, pattern [, start [, flags]])` — non-overlapping
/// matches at or after the 1-based character position `start`.
pub fn regexp_count(s: &str, pattern: &str, start: i32, flags: &str) -> Result<i32> {
    let offset = start_offset(s, start)?;
    let (opts, global) = parse_re_flags(flags)?;
    if global {
        return Err(reject_global("regexp_count"));
    }
    let Some(offset) = offset else {
        return Ok(0);
    };
    with_cached(pattern, PatternKind::Regex(opts), |re| {
        // PG re-seeds the non-overlapping scan *at* `start`, so a match that
        // began earlier is re-found clipped rather than skipped. `find_at`
        // keeps `^` and look-behind aware of the text before `start`, which
        // slicing the haystack would not.
        let mut cursor = offset;
        let mut count: i32 = 0;
        while let Some(m) = re.find_at(s, cursor) {
            count = count.saturating_add(1);
            cursor = advance(s, &m, cursor);
            if cursor > s.len() {
                break;
            }
        }
        count
    })
}

/// `regexp_substr(string, pattern [, start [, n [, flags [, subexpr]]]])` — the
/// `n`th match at or after `start`, or its `subexpr`th capture group. Returns
/// `None` (SQL NULL) when there is no such match or the group did not
/// participate.
pub fn regexp_substr(
    s: &str,
    pattern: &str,
    start: i32,
    n: i32,
    flags: &str,
    subexpr: i32,
) -> Result<Option<String>> {
    let offset = start_offset(s, start)?;
    if n < 1 {
        return Err(invalid_parameter("n", n));
    }
    if subexpr < 0 {
        return Err(invalid_parameter("subexpr", subexpr));
    }
    let (opts, global) = parse_re_flags(flags)?;
    if global {
        return Err(reject_global("regexp_substr"));
    }
    let Some(offset) = offset else {
        return Ok(None);
    };
    with_cached(pattern, PatternKind::Regex(opts), |re| {
        // Walk to the `n`th match the same way `regexp_count` counts them.
        let mut cursor = offset;
        for _ in 1..n {
            let m = re.find_at(s, cursor)?;
            cursor = advance(s, &m, cursor);
            if cursor > s.len() {
                return None;
            }
        }
        let caps = re.captures_at(s, cursor)?;
        // A pattern with no subexpressions has no group to ask for, so PG
        // treats `subexpr` 1 as the whole match. Anything genuinely out of
        // range, or a group that did not participate, is NULL rather than an
        // error.
        let group = if subexpr == 1 && re.captures_len() == 1 {
            0
        } else {
            subexpr as usize
        };
        caps.get(group).map(|m| m.as_str().to_string())
    })
}

/// Emit `c` as a regex literal, escaping it when it is a regex metacharacter
/// (`regex::escape` covers the full set: `. + * ? ( ) | [ ] { } ^ $ \`).
fn push_literal(out: &mut String, c: char) {
    // `regex::escape` is the authoritative metacharacter set, so a literal char
    // stays a literal even as the regex grammar grows.
    let mut buf = [0u8; 4];
    out.push_str(&regex::escape(c.encode_utf8(&mut buf)));
}

/// Copy a bracket expression `[...]` verbatim from `chars` (positioned just
/// after the opening `[`) into `out`. Inside a bracket expression the SIMILAR TO
/// wildcards `%`/`_` and the escape character lose their meaning — PG hands the
/// contents to its regex engine as a POSIX character class — so we pass them
/// through unchanged (only tracking where the class ends). A leading `^` and a
/// `]` in first position are literal members, and `[:name:]`/`[.x.]`/`[=x=]`
/// sub-expressions are copied whole. An unterminated class is left unbalanced so
/// the regex compiler rejects it, matching PG's `brackets [] not balanced`.
fn copy_bracket(chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String) {
    out.push('[');
    if chars.peek() == Some(&'^') {
        out.push('^');
        chars.next();
    }
    if chars.peek() == Some(&']') {
        out.push(']');
        chars.next();
    }
    while let Some(c) = chars.next() {
        match c {
            ']' => {
                out.push(']');
                return;
            }
            '[' if matches!(chars.peek(), Some(':' | '.' | '=')) => {
                // A POSIX class/collating/equivalence sub-expression: copy it
                // through its matching `:]` / `.]` / `=]` so the inner `]` does
                // not prematurely close the outer class.
                let Some(&kind) = chars.peek() else {
                    break;
                };
                out.push('[');
                out.push(kind);
                chars.next();
                while let Some(cc) = chars.next() {
                    out.push(cc);
                    if cc == kind && chars.peek() == Some(&']') {
                        out.push(']');
                        chars.next();
                        break;
                    }
                }
            }
            other => out.push(other),
        }
    }
}

/// If a valid regex bound (`{m}`, `{m,}`, `{m,n}`) follows the just-consumed
/// `{`, copy it through the closing `}` and return; otherwise emit a literal
/// `{`, leaving the following characters for the caller. PG treats a `{` that
/// does not open a bound as an ordinary character.
fn push_brace(chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String) {
    let mut look = chars.clone();
    let mut bound = String::new();
    let digits = |it: &mut std::iter::Peekable<std::str::Chars>, buf: &mut String| {
        let mut any = false;
        while let Some(&d) = it.peek() {
            if d.is_ascii_digit() {
                buf.push(d);
                it.next();
                any = true;
            } else {
                break;
            }
        }
        any
    };
    if !digits(&mut look, &mut bound) {
        push_literal(out, '{');
        return;
    }
    if look.peek() == Some(&',') {
        bound.push(',');
        look.next();
        digits(&mut look, &mut bound);
    }
    if look.peek() == Some(&'}') {
        look.next();
        out.push('{');
        out.push_str(&bound);
        out.push('}');
        *chars = look;
    } else {
        push_literal(out, '{');
    }
}

/// Translate a `SIMILAR TO` pattern into an anchored POSIX regex string.
///
/// `%` becomes `.*` and `_` becomes `.` (both match any character, including a
/// newline — hence the leading `(?s)`); the SQL-regex metacharacters `| * + ? (
/// )` and valid `{...}` bounds pass through; bracket expressions `[...]` are
/// copied verbatim (see [`copy_bracket`]); every other character is emitted as a
/// regex literal. The escape character (default `\`) makes the following
/// character a literal. The result is wrapped in `(?s)^(?:...)$` so the match
/// spans the whole string.
fn similar_to_regex(pattern: &str, escape: Option<char>) -> Result<String> {
    let mut out = String::from("(?s)^(?:");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            match chars.next() {
                Some(next) => push_literal(&mut out, next),
                None => {
                    return Err(TextError::new(
                        sqlstate::INVALID_ESCAPE_SEQUENCE,
                        "invalid SQL regular expression: escape character must not be the last character",
                    ));
                }
            }
        } else {
            match c {
                '%' => out.push_str(".*"),
                '_' => out.push('.'),
                // SQL-regex metacharacters shared with POSIX regex.
                '|' | '*' | '+' | '?' | '(' | ')' => out.push(c),
                '[' => copy_bracket(&mut chars, &mut out),
                '{' => push_brace(&mut chars, &mut out),
                other => push_literal(&mut out, other),
            }
        }
    }
    out.push_str(")$");
    Ok(out)
}

// --- encode / decode -------------------------------------------------------

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `encode(bytea, format)`: `hex`, `base64` (wrapped at 76 columns like PG), or
/// `escape`.
pub fn encode(bytes: &[u8], format: &str) -> Result<String> {
    match format {
        "hex" => Ok(bytes.iter().map(|b| format!("{b:02x}")).collect()),
        "base64" => Ok(encode_base64(bytes)),
        "escape" => Ok(encode_escape(bytes)),
        other => Err(TextError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("unrecognized encoding: \"{other}\""),
        )),
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut col = 0;
    let push = |out: &mut String, col: &mut usize, ch: char| {
        // PG wraps base64 output every 76 characters with a newline.
        if *col == 76 {
            out.push('\n');
            *col = 0;
        }
        out.push(ch);
        *col += 1;
    };
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        push(&mut out, &mut col, BASE64[(n >> 18) as usize & 63] as char);
        push(&mut out, &mut col, BASE64[(n >> 12) as usize & 63] as char);
        push(
            &mut out,
            &mut col,
            if chunk.len() > 1 {
                BASE64[(n >> 6) as usize & 63] as char
            } else {
                '='
            },
        );
        push(
            &mut out,
            &mut col,
            if chunk.len() > 2 {
                BASE64[n as usize & 63] as char
            } else {
                '='
            },
        );
    }
    out
}

fn encode_escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out
}

/// `decode(text, format)`: inverse of [`encode`].
pub fn decode(s: &str, format: &str) -> Result<Vec<u8>> {
    match format {
        "hex" => decode_hex(s),
        "base64" => decode_base64(s),
        "escape" => Ok(decode_escape(s)),
        other => Err(TextError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            format!("unrecognized encoding: \"{other}\""),
        )),
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut hi: Option<u8> = None;
    for c in s.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let v = hex_val(c).ok_or_else(|| {
            TextError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!("invalid hexadecimal digit: \"{}\"", c as char),
            )
        })?;
        match hi.take() {
            None => hi = Some(v),
            Some(h) => out.push((h << 4) | v),
        }
    }
    if hi.is_some() {
        return Err(TextError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid hexadecimal data: odd number of digits",
        ));
    }
    Ok(out)
}

fn base64_val(c: u8) -> Option<u8> {
    BASE64.iter().position(|&x| x == c).map(|p| p as u8)
}

fn decode_base64(s: &str) -> Result<Vec<u8>> {
    let end_seq = || {
        TextError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "invalid base64 end sequence",
        )
    };
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    let mut ndata = 0usize; // significant (non-pad) symbols seen
    let mut npad = 0usize; // trailing '=' padding symbols
    for c in s.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            npad += 1;
            continue;
        }
        // A data symbol after padding is a malformed sequence.
        if npad > 0 {
            return Err(end_seq());
        }
        let v = base64_val(c).ok_or_else(|| {
            TextError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                format!(
                    "invalid symbol \"{}\" found while decoding base64 sequence",
                    c as char
                ),
            )
        })?;
        buf = (buf << 6) | v as u32;
        bits += 6;
        ndata += 1;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    // The full symbol count must be a multiple of 4, a group can't end on a lone
    // data symbol, and at most two '=' pad it — otherwise PG rejects the input.
    if (ndata + npad) % 4 != 0 || ndata % 4 == 1 || npad > 2 {
        return Err(end_seq());
    }
    Ok(out)
}

fn decode_escape(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'\\' {
                out.push(b'\\');
                i += 2;
                continue;
            }
            // \ooo octal escape
            if i + 3 < bytes.len() + 1
                && (b'0'..=b'7').contains(&bytes[i + 1])
                && i + 3 < bytes.len() + 1
            {
                let oct = &bytes[i + 1..(i + 4).min(bytes.len())];
                if oct.len() == 3 && oct.iter().all(|b| (b'0'..=b'7').contains(b)) {
                    let val = (oct[0] - b'0') * 64 + (oct[1] - b'0') * 8 + (oct[2] - b'0');
                    out.push(val);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

// --- quoting / format ------------------------------------------------------

/// `quote_ident`: double-quote the identifier if it is not a bare lowercase
/// identifier.
pub fn quote_ident(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_lowercase() || (i > 0 && c.is_ascii_digit()));
    if bare {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// `quote_literal`: single-quote the string, doubling embedded quotes; use an
/// `E'...'` string and double backslashes when the input contains a backslash.
pub fn quote_literal(s: &str) -> String {
    let quotes = s.replace('\'', "''");
    if s.contains('\\') {
        format!("E'{}'", quotes.replace('\\', "\\\\"))
    } else {
        format!("'{quotes}'")
    }
}

/// `quote_nullable`: `quote_literal`, or the literal `NULL` for a NULL input.
pub fn quote_nullable(s: Option<&str>) -> String {
    match s {
        Some(s) => quote_literal(s),
        None => "NULL".to_string(),
    }
}

/// A `format()` argument: its text representation (`None` for SQL NULL).
pub type FormatArg = Option<String>;

/// `format(fmtstr, args...)`: supports `%s`, `%I`, `%L`, `%%`, positional
/// `%n$` arguments, and a field width (`%10s`, `%-10s`, `%*s`, `%*n$s`).
pub fn format(fmt: &str, args: &[FormatArg]) -> Result<String> {
    let unterminated = || {
        TextError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "unterminated format() type specifier",
        )
    };
    // The argument at 1-based position `n` (0 is rejected as PG does).
    let arg_at = |n: usize| -> Result<&FormatArg> {
        if n == 0 {
            return Err(TextError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "format specifies argument 0, but arguments are numbered from 1",
            ));
        }
        args.get(n - 1).ok_or_else(|| {
            TextError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "too few arguments for format()",
            )
        })
    };
    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    let mut auto = 1usize; // next auto-assigned 1-based argument
    // Scan a run of digits followed by `$`; consume and return it as a 1-based
    // position, or leave `i` unmoved when the run isn't an argument selector.
    let scan_arg_pos = |chars: &[char], i: &mut usize| -> Option<usize> {
        let start = *i;
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i > start && *i < chars.len() && chars[*i] == '$' {
            let n: usize = chars[start..*i]
                .iter()
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            *i += 1;
            Some(n)
        } else {
            *i = start;
            None
        }
    };
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            return Err(unterminated());
        }
        if chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        // [argpos '$']
        let explicit = scan_arg_pos(&chars, &mut i);
        // [flags] — only '-' (left-justify)
        let mut left_justify = false;
        while i < chars.len() && chars[i] == '-' {
            left_justify = true;
            i += 1;
        }
        // [width] — a literal digit run, or `*` / `*n$` reading an int argument
        let mut width: Option<i64> = None;
        if i < chars.len() && chars[i] == '*' {
            i += 1;
            let pos = scan_arg_pos(&chars, &mut i).unwrap_or_else(|| {
                let a = auto;
                auto += 1;
                a
            });
            // A null width is treated as no width, matching PG.
            let w: i64 = arg_at(pos)?
                .as_deref()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            width = Some(w);
        } else if i < chars.len() && chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            width = chars[start..i].iter().collect::<String>().parse().ok();
        }
        // A negative width means left-justify to its absolute value.
        if let Some(w) = width
            && w < 0
        {
            left_justify = true;
            width = Some(-w);
        }
        if i >= chars.len() {
            return Err(unterminated());
        }
        let conv = chars[i];
        i += 1;
        let pos = explicit.unwrap_or_else(|| {
            let a = auto;
            auto += 1;
            a
        });
        let arg = arg_at(pos)?;
        let piece = match conv {
            's' => arg.as_deref().unwrap_or("").to_string(),
            'I' => match arg {
                Some(v) => quote_ident(v),
                None => {
                    return Err(TextError::new(
                        sqlstate::NULL_VALUE_NOT_ALLOWED,
                        "null values cannot be formatted as an SQL identifier",
                    ));
                }
            },
            'L' => quote_nullable(arg.as_deref()),
            other => {
                return Err(TextError::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!("unrecognized format() type specifier \"{other}\""),
                ));
            }
        };
        // Pad the formatted piece to the requested field width with spaces.
        let pad = width
            .map(|w| w as usize)
            .unwrap_or(0)
            .saturating_sub(char_length(&piece) as usize);
        if pad > 0 && !left_justify {
            out.extend(std::iter::repeat(' ').take(pad));
        }
        out.push_str(&piece);
        if pad > 0 && left_justify {
            out.extend(std::iter::repeat(' ').take(pad));
        }
    }
    Ok(out)
}

// --- character-type length coercion (varchar / char / name) ----------------

/// Truncate a value to `n` characters for an *explicit* cast to
/// `varchar(n)`/`char(n)` — no length error, just truncation.
pub fn truncate_chars(s: &str, n: i32) -> String {
    if n <= 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n as usize {
        s.to_string()
    } else {
        chars[..n as usize].iter().collect()
    }
}

/// `varchar(n)` length coercion. In *explicit* cast context an over-long value
/// is silently truncated; in *assignment/implicit* context it errors unless the
/// excess characters are all spaces (then it is truncated).
pub fn varchar_input(s: &str, n: i32, explicit: bool) -> Result<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n.max(0) as usize {
        return Ok(s.to_string());
    }
    if explicit {
        return Ok(chars[..n.max(0) as usize].iter().collect());
    }
    if chars[n.max(0) as usize..].iter().all(|&c| c == ' ') {
        Ok(chars[..n.max(0) as usize].iter().collect())
    } else {
        Err(TextError::new(
            sqlstate::STRING_DATA_RIGHT_TRUNCATION,
            format!("value too long for type character varying({n})"),
        ))
    }
}

/// `char(n)` / `bpchar(n)` length coercion: blank-pad to `n`; over-long values
/// truncate on explicit cast, and on assignment error unless the excess is all
/// spaces.
pub fn bpchar_input(s: &str, n: i32, explicit: bool) -> Result<String> {
    let n = n.max(0) as usize;
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > n {
        if !explicit && !chars[n..].iter().all(|&c| c == ' ') {
            return Err(TextError::new(
                sqlstate::STRING_DATA_RIGHT_TRUNCATION,
                format!("value too long for type character({n})"),
            ));
        }
        return Ok(chars[..n].iter().collect());
    }
    let mut out: String = s.to_string();
    out.extend(std::iter::repeat(' ').take(n - chars.len()));
    Ok(out)
}

/// The trailing-blank-insensitive text of a `bpchar` value (its cast to
/// `text`, as used by `||` and `::text`).
pub fn bpchar_rtrim(s: &str) -> String {
    s.trim_end_matches(' ').to_string()
}

/// `name` input: truncate to 63 characters (`NAMEDATALEN - 1`).
pub fn name_input(s: &str) -> String {
    truncate_chars(s, 63)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn lengths() {
        assert_eq!(char_length("café"), 4);
        assert_eq!(octet_length("é"), 2);
        assert_eq!(bit_length("abc"), 24);
    }

    #[test]
    fn case_and_initcap() {
        assert_eq!(initcap("hi THERE o'brien"), "Hi There O'Brien");
        assert_eq!(initcap("123abc def"), "123abc Def");
    }

    #[test]
    fn substr_semantics() -> anyhow::Result<()> {
        assert_eq!(substr("abcdef", 2, Some(3))?, "bcd");
        assert_eq!(substr("café", 2, None)?, "afé");
        assert_eq!(substr("abcdef", 0, Some(2))?, "a");
        assert_eq!(substr("abc", 2, Some(-1)).unwrap_err().sqlstate, "22011");

        Ok(())
    }

    #[test]
    fn strpos_and_overlay() -> anyhow::Result<()> {
        assert_eq!(strpos("abcabc", "bc"), 2);
        assert_eq!(strpos("abc", ""), 1);
        assert_eq!(strpos("abc", "z"), 0);
        assert_eq!(overlay("Txxxxas", "hom", 2, Some(4))?, "Thomas");
        // A start of 0 (or below) is a negative-substring error, as in PG.
        assert_eq!(overlay("abc", "X", 0, None).unwrap_err().sqlstate, "22011");

        Ok(())
    }

    #[test]
    fn pad_and_trim() -> anyhow::Result<()> {
        assert_eq!(pad("abcdef", 3, " ", true)?, "abc");
        assert_eq!(pad("ab", 5, "xy", false)?, "abxyx");
        assert_eq!(pad("ab", 5, "", true)?, "ab");
        assert_eq!(pad("abc", -1, " ", true)?, "");
        // A length past MaxAllocSize is rejected instead of allocating.
        assert_eq!(
            pad("a", 2_000_000_000, "x", true).unwrap_err().sqlstate,
            "54000"
        );
        assert_eq!(trim("xxabcxx", "x", TrimSide::Both), "abc");

        Ok(())
    }

    #[test]
    fn translate_replace_repeat() -> anyhow::Result<()> {
        assert_eq!(translate("12345", "143", "ax"), "a2x5");
        assert_eq!(replace("abcabc", "", "X"), "abcabc");
        assert_eq!(repeat("x", -2)?, "");
        assert_eq!(repeat("x", 3)?, "xxx");
        assert_eq!(repeat("ab", 2_000_000_000).unwrap_err().sqlstate, "54000");
        assert_eq!(reverse("café"), "éfac");

        Ok(())
    }

    #[test]
    fn left_right_split() -> anyhow::Result<()> {
        assert_eq!(left("abc", -1), "ab");
        assert_eq!(right("abc", -1), "bc");
        assert_eq!(split_part("a,b,c", ",", -1)?, "c");
        assert_eq!(split_part("abc", "", 1)?, "abc");
        assert_eq!(split_part("a", ",", 0).unwrap_err().sqlstate, "22023");

        Ok(())
    }

    #[test]
    fn chr_and_ascii() {
        assert_eq!(ascii(""), 0);
        assert_eq!(chr(0).unwrap_err().sqlstate, "54000");
        assert_eq!(chr(-1).unwrap_err().sqlstate, "22023");
        assert_eq!(chr(1114112).unwrap_err().sqlstate, "54000");
        // A surrogate code point is "not valid", distinct from "too large".
        assert_eq!(
            chr(55296).unwrap_err().message,
            "requested character not valid for encoding: 55296"
        );
        assert_eq!(to_hex_i32(-1), "ffffffff");
    }

    #[test]
    fn like_matching() -> anyhow::Result<()> {
        assert!(like("abc", "a%", None, false)?);
        assert!(like("abc", "a_c", None, false)?);
        assert!(!like("abc", "a_", None, false)?);
        assert!(like("ABC", "abc", None, true)?);
        assert!(like("a%b", "a\\%b", Some('\\'), false)?);
        assert!(!like("axb", "a\\%b", Some('\\'), false)?);

        Ok(())
    }

    #[test]
    fn regex_matching() -> anyhow::Result<()> {
        // `~` is an unanchored (substring) match.
        assert!(regex_match("abc", "b", false)?);
        assert!(regex_match("abc", "^a", false)?);
        assert!(!regex_match("abc", "^b", false)?);
        assert!(!regex_match("abc", "z", false)?);
        // `~*` is case-insensitive.
        assert!(regex_match("ABC", "abc", true)?);
        assert!(!regex_match("ABC", "abc", false)?);
        // A malformed pattern raises `invalid regular expression` (2201B).
        let e = regex_match("abc", "a(", false).unwrap_err();
        assert_eq!(e.sqlstate, "2201B");
        assert!(e.message.starts_with("invalid regular expression:"));

        Ok(())
    }

    /// PG's regexes are newline-*insensitive* by default: `.` spans newlines
    /// and the anchors bind to the whole string, the opposite of the `regex`
    /// crate's defaults.
    #[test]
    fn regex_newline_defaults() -> anyhow::Result<()> {
        assert!(regex_match("a\nb", "a.b", false)?);
        assert_eq!(regexp_replace("a\nb", "a.b", "X", "")?, "X");
        // `n`/`m` make both `.` and the anchors newline-aware.
        assert_eq!(regexp_replace("a\nb", "a.b", "X", "n")?, "a\nb");
        assert_eq!(regexp_replace("a\nb", "^b", "X", "n")?, "a\nX");
        // `p` is newline-sensitive for `.` only, `w` for the anchors only.
        assert_eq!(regexp_replace("a\nb", "^b", "X", "p")?, "a\nb");
        assert_eq!(regexp_replace("a\nb", "a.b", "X", "w")?, "X");

        Ok(())
    }

    #[test]
    fn regex_flags() -> anyhow::Result<()> {
        assert_eq!(regexp_replace("abc", "B", "X", "i")?, "aXc");
        // A later flag overrides an earlier one.
        assert_eq!(regexp_replace("abc", "B", "X", "ic")?, "abc");
        // `x` ignores whitespace in the pattern; `q` makes it a literal.
        assert_eq!(regexp_replace("abc", "a b c", "X", "x")?, "X");
        assert_eq!(regexp_replace("a.c", "a.c", "X", "q")?, "X");
        assert_eq!(regexp_replace("abc", "a.c", "X", "q")?, "abc");
        // An unknown flag is 22023; `b` (BRE) is an unimplemented feature.
        let e = regexp_replace("abc", "b", "X", "z").unwrap_err();
        assert_eq!(e.sqlstate, "22023");
        assert_eq!(e.message, "invalid regular expression option: \"z\"");
        assert_eq!(
            regexp_replace("abc", "b", "X", "b").unwrap_err().sqlstate,
            "0A000"
        );

        Ok(())
    }

    #[test]
    fn regexp_replace_substitutions() -> anyhow::Result<()> {
        // Only the first match without `g`, every match with it.
        assert_eq!(regexp_replace("a1b2", "[0-9]", "X", "")?, "aXb2");
        assert_eq!(regexp_replace("a1b2", "[0-9]", "X", "g")?, "aXbX");
        // `\1`..`\9` are capture groups and `\&` is the whole match.
        assert_eq!(
            regexp_replace("1112223333", r"(\d{3})(\d{3})(\d{4})", r"(\1) \2-\3", "")?,
            "(111) 222-3333"
        );
        assert_eq!(regexp_replace("abc", "b", r"[\&]", "")?, "a[b]c");
        // `\\` is one literal backslash.
        assert_eq!(regexp_replace("abc", "b", r"\\", "")?, r"a\c");
        // Only one digit is consumed, so `\10` is group 1 then a literal `0`.
        assert_eq!(regexp_replace("abc", "(b)", r"[\10]", "")?, "a[b0]c");
        // A reference to a group that does not exist expands to nothing.
        assert_eq!(regexp_replace("abc", "(b)", r"[\9]", "")?, "a[]c");
        // Any other escape keeps both characters, and so does a trailing one.
        assert_eq!(regexp_replace("abc", "b", r"[\q]", "")?, r"a[\q]c");
        assert_eq!(regexp_replace("abc", "b", r"x\", "")?, r"ax\c");
        // `$` is a metacharacter for the `regex` crate but a literal for PG.
        assert_eq!(regexp_replace("abc", "b", "$1", "")?, "a$1c");

        Ok(())
    }

    #[test]
    fn regexp_like_count_substr() -> anyhow::Result<()> {
        assert!(regexp_like("abc", "B", "i")?);
        assert!(!regexp_like("abc", "B", "")?);

        assert_eq!(regexp_count("abcabc", "a", 1, "")?, 2);
        assert_eq!(regexp_count("abcABC", "a", 1, "i")?, 2);
        assert_eq!(regexp_count("abcabc", "a", 2, "")?, 1);
        // The scan re-seeds *at* `start` rather than filtering a scan that
        // began at 0, so a match overlapping an earlier one is still found.
        assert_eq!(regexp_count("aaaaa", "aa", 2, "")?, 2);
        // The engine still sees the text before `start`, so `^` stays bound to
        // the start of the whole string.
        assert_eq!(regexp_count("abcabc", "^a", 2, "")?, 0);
        // A `start` past the end of the string simply finds nothing.
        assert_eq!(regexp_count("abc", "b", 9, "")?, 0);
        // An empty match must advance the cursor, not stall it.
        assert_eq!(regexp_count("abc", "", 1, "")?, 4);

        assert_eq!(
            regexp_substr("abcdef", "c.", 1, 1, "", 0)?.as_deref(),
            Some("cd")
        );
        assert_eq!(
            regexp_substr("abcabc", "b", 2, 1, "", 0)?.as_deref(),
            Some("b")
        );
        // The `n`th match, and a capture group within it.
        assert_eq!(
            regexp_substr("foobarbaz", "b(a)(.)", 1, 2, "i", 2)?.as_deref(),
            Some("z")
        );
        // A match that began before `start` is re-found clipped, not skipped.
        assert_eq!(
            regexp_substr("aaaaa", "aa", 2, 1, "", 0)?.as_deref(),
            Some("aa")
        );
        assert_eq!(
            regexp_substr("aaaaa", "aa", 2, 2, "", 0)?.as_deref(),
            Some("aa")
        );
        assert_eq!(
            regexp_substr("hello world", "[a-z]+", 3, 1, "", 0)?.as_deref(),
            Some("llo")
        );
        // A pattern with no capture groups has no group to ask for, so PG
        // treats `subexpr` 1 as the whole match.
        assert_eq!(
            regexp_substr("abc", "b", 1, 1, "", 1)?.as_deref(),
            Some("b")
        );
        // No match, an out-of-range group and a group that did not participate
        // are all NULL rather than errors.
        assert_eq!(regexp_substr("abc", "z", 1, 1, "", 0)?, None);
        assert_eq!(regexp_substr("abc", "b", 9, 1, "", 0)?, None);
        assert_eq!(regexp_substr("abc", "b", 1, 1, "", 2)?, None);
        assert_eq!(regexp_substr("abc", "(b)", 1, 1, "", 5)?, None);
        assert_eq!(regexp_substr("abc", "(x)?b", 1, 1, "", 1)?, None);

        Ok(())
    }

    /// The `regex` crate cannot express these two through builder options, so
    /// [`rewrite_pattern`] rewrites the pattern instead.
    #[test]
    fn regex_pattern_rewrites() -> anyhow::Result<()> {
        // Expanded mode ignores whitespace and `#` comments, but *not* inside a
        // bracket expression.
        assert!(regexp_like("a b", "a[ ]b", "x")?);
        assert!(regexp_like("ab", "a b", "x")?);
        assert!(regexp_like("ab", "a#comment\nb", "x")?);
        // An escaped space stays literal even in expanded mode.
        assert!(regexp_like("a b", r"a\ b", "x")?);
        // Under a newline-sensitive mode a negated class must not match a
        // newline; under the default (and `w`) it must.
        assert!(!regexp_like("\n", "[^x]", "n")?);
        assert!(!regexp_like("\n", "[^x]", "p")?);
        assert!(regexp_like("\n", "[^x]", "")?);
        assert!(regexp_like("\n", "[^x]", "w")?);
        assert_eq!(regexp_replace("a\nb", "[^x]b", "X", "n")?, "a\nb");
        // Wrapping the class (rather than injecting into it) keeps a leading or
        // trailing `-` literal instead of turning it into a range.
        assert!(!regexp_like("-", "[^-a]", "n")?);
        assert!(!regexp_like("-", "[^a-]", "n")?);
        assert!(regexp_like("b", "[^a-]", "n")?);
        // A positive class that names a newline still matches one.
        assert!(regexp_like("\n", "[\\n]", "n")?);

        Ok(())
    }

    /// jsonpath's `like_regex` is XQuery-flavored: `.` does not span a newline
    /// unless `s` is given, the opposite of the `~` operator's default.
    #[test]
    fn jsonpath_like_regex_flags() -> anyhow::Result<()> {
        assert!(!like_regex_match("a\nb", "a.b", "")?);
        assert!(like_regex_match("a\nb", "a.b", "s")?);
        assert!(like_regex_match("ABC", "abc", "i")?);
        assert!(!like_regex_match("ABC", "abc", "")?);
        // `~` keeps the POSIX default, so the two disagree by design.
        assert!(regex_match("a\nb", "a.b", false)?);

        Ok(())
    }

    #[test]
    fn regexp_parameter_errors() {
        // These functions match at most once, so `g` is rejected — and PG
        // rejects it before it even compiles the pattern.
        for e in [
            regexp_like("abc", "a(", "g").unwrap_err(),
            regexp_count("abc", "a(", 1, "g").unwrap_err(),
            regexp_substr("abc", "a(", 1, 1, "g", 0).unwrap_err(),
        ] {
            assert_eq!(e.sqlstate, "22023");
            assert!(
                e.message
                    .ends_with("does not support the \"global\" option")
            );
        }
        assert_eq!(
            regexp_count("abc", "b", 0, "").unwrap_err().message,
            "invalid value for parameter \"start\": 0"
        );
        assert_eq!(
            regexp_substr("abc", "b", 1, 0, "", 0).unwrap_err().message,
            "invalid value for parameter \"n\": 0"
        );
        assert_eq!(
            regexp_substr("abc", "b", 1, 1, "", -1).unwrap_err().message,
            "invalid value for parameter \"subexpr\": -1"
        );
    }

    #[test]
    fn similar_to_matching() -> anyhow::Result<()> {
        // SIMILAR TO matches the whole string (anchored).
        assert!(similar_to_match("abc", "a%", Some('\\'))?);
        assert!(!similar_to_match("abc", "a", Some('\\'))?);
        assert!(similar_to_match("abc", "a_c", Some('\\'))?);
        // Alternation and grouping are SQL-regex metacharacters.
        assert!(similar_to_match("abc", "(a|z)%", Some('\\'))?);
        // A literal `.` must not act as a wildcard.
        assert!(!similar_to_match("axc", "a.c", Some('\\'))?);
        assert!(similar_to_match("a.c", "a.c", Some('\\'))?);
        // The escape character makes the next character a literal.
        assert!(similar_to_match("a%c", "a\\%c", Some('\\'))?);
        assert!(!similar_to_match("axc", "a\\%c", Some('\\'))?);
        // Escaping a metacharacter shared with regex keeps it literal (not raw).
        assert!(similar_to_match("a|b", "a\\|b", Some('\\'))?);
        assert!(!similar_to_match("a", "a\\|b", Some('\\'))?);
        assert!(similar_to_match("(", "\\(", Some('\\'))?);
        // A trailing bare escape is an error.
        assert_eq!(
            similar_to_match("abc", "abc\\", Some('\\'))
                .unwrap_err()
                .sqlstate,
            "22025"
        );

        Ok(())
    }

    #[test]
    fn similar_to_bracket_expressions() -> anyhow::Result<()> {
        // Inside `[...]`, `%` and `_` are literal members, not wildcards.
        assert!(similar_to_match("%", "[%_]", Some('\\'))?);
        assert!(similar_to_match("_", "[%_]", Some('\\'))?);
        assert!(!similar_to_match("x", "[%_]", Some('\\'))?);
        assert!(similar_to_match("_", "[_]", Some('\\'))?);
        assert!(!similar_to_match(".", "[_]", Some('\\'))?);
        // Negated classes work (a leading `^` is class negation, not a literal).
        assert!(similar_to_match("b", "[^a]", Some('\\'))?);
        assert!(!similar_to_match("a", "[^a]", Some('\\'))?);
        // Ranges and POSIX classes pass through.
        assert!(similar_to_match("-", "[a-]", Some('\\'))?);
        assert!(similar_to_match("a", "[[:alpha:]]", Some('\\'))?);
        assert!(!similar_to_match("1", "[[:alpha:]]", Some('\\'))?);
        // An unbalanced bracket is rejected, as in PG.
        assert_eq!(
            similar_to_match("a", "a[", Some('\\'))
                .unwrap_err()
                .sqlstate,
            "2201B"
        );

        Ok(())
    }

    #[test]
    fn similar_to_newline_and_braces() -> anyhow::Result<()> {
        // `%` and `_` match any character, including a newline.
        assert!(similar_to_match("a\nb", "a%b", Some('\\'))?);
        assert!(similar_to_match("a\nb", "a_b", Some('\\'))?);
        // A valid bound is a quantifier; an invalid `{` is a literal.
        assert!(similar_to_match("aa", "a{2}", Some('\\'))?);
        assert!(!similar_to_match("a", "a{2}", Some('\\'))?);
        assert!(similar_to_match("a{c", "a{c", Some('\\'))?);
        assert!(!similar_to_match("ac", "a{c", Some('\\'))?);

        Ok(())
    }

    #[test]
    fn encode_decode_roundtrip() -> anyhow::Result<()> {
        assert_eq!(encode(&[0x00, 0x10, 0x00], "hex")?, "001000");
        assert_eq!(encode(b"abc", "base64")?, "YWJj");
        assert_eq!(encode(b"a\x00b", "escape")?, "a\\000b");
        assert_eq!(decode("YWJj", "base64")?, b"abc");
        assert_eq!(decode("001000", "hex")?, vec![0x00, 0x10, 0x00]);
        // Malformed base64 (missing padding / lone trailing symbol) is rejected.
        assert_eq!(
            decode("abc", "base64").unwrap_err().message,
            "invalid base64 end sequence"
        );
        assert_eq!(
            decode("a@b", "base64").unwrap_err().message,
            "invalid symbol \"@\" found while decoding base64 sequence"
        );
        assert_eq!(
            decode("xy", "hex").unwrap_err().message,
            "invalid hexadecimal digit: \"x\""
        );

        Ok(())
    }

    #[test]
    fn quoting_and_format() -> anyhow::Result<()> {
        assert_eq!(quote_ident("foo bar"), "\"foo bar\"");
        assert_eq!(quote_ident("foo"), "foo");
        assert_eq!(quote_literal("a\\b'c"), "E'a\\\\b''c'");
        assert_eq!(quote_nullable(None), "NULL");
        assert_eq!(
            format(
                "%s-%I-%L",
                &[Some("a".into()), Some("b c".into()), Some("d'e".into())]
            )?,
            "a-\"b c\"-'d''e'"
        );
        assert_eq!(format("%1$s %1$s", &[Some("x".into())])?, "x x");
        // Field width: right- and left-justified, and `*` reading a width arg.
        assert_eq!(format("%5s|", &[Some("x".into())])?, "    x|");
        assert_eq!(format("%-5s|", &[Some("x".into())])?, "x    |");
        assert_eq!(format("%*s", &[Some("3".into()), Some("x".into())])?, "  x");
        assert_eq!(format("%%", &[])?, "%");
        assert_eq!(
            format("%", &[]).unwrap_err().message,
            "unterminated format() type specifier"
        );
        assert_eq!(
            format("%0$s", &[Some("x".into())]).unwrap_err().message,
            "format specifies argument 0, but arguments are numbered from 1"
        );

        Ok(())
    }

    #[test]
    fn char_length_coercion() -> anyhow::Result<()> {
        assert_eq!(bpchar_input("ab", 5, false)?, "ab   ");
        assert_eq!(varchar_input("abcdef", 3, true)?, "abc");
        assert_eq!(varchar_input("ab   ", 2, false)?, "ab");
        assert_eq!(
            varchar_input("abcdef", 3, false).unwrap_err().sqlstate,
            "22001"
        );
        assert_eq!(bpchar_rtrim("ab   "), "ab");

        Ok(())
    }
}
