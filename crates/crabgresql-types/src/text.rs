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
    pub const INVALID_USE_OF_ESCAPE_CHARACTER: &str = "2200C";
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

/// One item of a `%`-free run: either literal text or a run of `_`.
enum SegItem {
    /// A literal run. Already lowercased when the program is case-insensitive.
    Lit(String),
    /// `n` consecutive `_`, each matching exactly one *character*.
    Skip(usize),
}

/// A `%`-free run of a pattern. Its width is fixed *because* it contains no
/// `%`, which is what makes the greedy scan in [`find_segment`] complete.
struct Segment {
    /// Alternating `Lit`/`Skip` — adjacent items of a kind are merged.
    items: Vec<SegItem>,
    /// Width in characters, not bytes.
    width: usize,
}

impl Segment {
    /// The segment's text when it is one literal run and nothing else. Drives
    /// the `Exact`/`Prefix`/`Suffix`/`Contains` specializations.
    fn as_lit(&self) -> Option<&str> {
        match self.items.as_slice() {
            [SegItem::Lit(s)] => Some(s),
            _ => None,
        }
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// A pattern compiled to the shape it actually has. The four string
/// specializations are degenerate `Segments`: there is one compiler, which
/// builds `Segments` and narrows at the end, so they cannot disagree.
enum LikeProgram {
    /// No wildcards: the subject must equal this text.
    Exact(String),
    /// `lit%`
    Prefix(String),
    /// `%lit`
    Suffix(String),
    /// `%lit%`
    Contains(String),
    /// No `%`, but at least one `_`: the whole subject is one segment.
    Whole(Segment),
    /// `head % mid … mid % tail`. `head`/`tail` are `None` when the pattern
    /// begins/ends with `%`; empty middle runs (from `%%`) are dropped.
    Segments {
        head: Option<Segment>,
        mids: Vec<Segment>,
        tail: Option<Segment>,
    },
}

/// Everything besides the pattern text that changes what it compiles to. Both
/// fields are load-bearing cache-key material: `'m%aca' ESCAPE '%'` compiles to
/// `Exact("maca")`, and the same text under `ESCAPE '\'` to two segments.
#[derive(Clone, Copy, PartialEq, Eq)]
struct LikeKind {
    escape: Option<char>,
    case_insensitive: bool,
}

/// How a compiled literal is compared against subject text. Two zero-sized
/// implementations rather than a runtime flag, so the hot loop carries no case
/// branch.
trait CaseCmp {
    fn eq(hay: &str, needle: &str) -> bool;
    fn starts_with(hay: &str, needle: &str) -> bool;
    fn ends_with(hay: &str, needle: &str) -> bool;
    /// Leftmost byte offset of `needle` in `hay`, or `None`.
    fn find(hay: &str, needle: &str) -> Option<usize>;
}

/// Byte-exact comparison, for `LIKE` and for the non-ASCII `ILIKE` fallback
/// (where the subject has already been lowercased in full).
struct Exact;

impl CaseCmp for Exact {
    fn eq(hay: &str, needle: &str) -> bool {
        hay == needle
    }
    fn starts_with(hay: &str, needle: &str) -> bool {
        hay.starts_with(needle)
    }
    fn ends_with(hay: &str, needle: &str) -> bool {
        hay.ends_with(needle)
    }
    fn find(hay: &str, needle: &str) -> Option<usize> {
        hay.find(needle)
    }
}

/// ASCII case folding, for `ILIKE` against an ASCII subject. The needle is
/// already lowercased (see [`compile_like`]), so only the haystack side folds.
///
/// Only ever reached with an all-ASCII `hay` (see [`like`]), so every byte
/// offset it produces is a character boundary. A needle carrying non-ASCII
/// simply never matches, which is the same answer the slow path reaches.
///
/// That precondition is the whole proof that comparing raw bytes is sound, and
/// it is established by the caller, so the methods assert it over the bytes
/// they actually read — a second caller that forgets (a batch kernel over an
/// Arrow `StringArray` is the obvious candidate) fails a test instead of
/// silently answering wrong. `Prefix`/`Suffix` only need their compared window
/// to be ASCII (see [`LikeProgram::ascii_window`]), which is why the assertion
/// lives on [`CaseCmp::eq`] rather than on the whole subject, and why the two
/// windowed methods index with `get` instead of slicing.
struct AsciiFold;

impl CaseCmp for AsciiFold {
    fn eq(hay: &str, needle: &str) -> bool {
        debug_assert!(hay.is_ascii(), "AsciiFold compared a non-ASCII window");
        hay.as_bytes().eq_ignore_ascii_case(needle.as_bytes())
    }
    fn starts_with(hay: &str, needle: &str) -> bool {
        hay.get(..needle.len()).is_some_and(|w| Self::eq(w, needle))
    }
    fn ends_with(hay: &str, needle: &str) -> bool {
        hay.len()
            .checked_sub(needle.len())
            .and_then(|at| hay.get(at..))
            .is_some_and(|w| Self::eq(w, needle))
    }
    fn find(hay: &str, needle: &str) -> Option<usize> {
        debug_assert!(hay.is_ascii(), "AsciiFold scanned a non-ASCII subject");
        let (hay, needle) = (hay.as_bytes(), needle.as_bytes());
        let Some(&first) = needle.first() else {
            return Some(0);
        };
        let last = hay.len().checked_sub(needle.len())?;
        // Scan for either case of the needle's first byte, then confirm. A
        // non-ASCII first byte has no other case, so one candidate suffices.
        let upper = first.to_ascii_uppercase();
        let mut from = 0;
        while from <= last {
            let window = &hay[from..=last];
            let hit = if first == upper {
                memchr::memchr(first, window)?
            } else {
                memchr::memchr2(first, upper, window)?
            };
            let at = from + hit;
            if hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
                return Some(at);
            }
            from = at + 1;
        }
        None
    }
}

/// Byte offset just past the character starting at `at`.
fn next_char(s: &str, at: usize) -> Option<usize> {
    Some(at + s[at..].chars().next()?.len_utf8())
}

/// Byte offset `n` characters past `at`, or `None` if the string ends first.
fn skip_chars(s: &str, at: usize, n: usize) -> Option<usize> {
    let mut at = at;
    for _ in 0..n {
        at = next_char(s, at)?;
    }
    Some(at)
}

/// Match `items` against `s` starting at `at`, returning the byte offset just
/// past the match. No backtracking is possible or needed: a `%`-free run has a
/// fixed width, so each item consumes a determined amount.
fn match_at<C: CaseCmp>(s: &str, at: usize, items: &[SegItem]) -> Option<usize> {
    let mut at = at;
    for item in items {
        match item {
            SegItem::Lit(lit) => {
                if !C::starts_with(s.get(at..)?, lit) {
                    return None;
                }
                // Safe because a successful `starts_with` under either policy
                // consumed exactly `lit.len()` bytes of the subject.
                at += lit.len();
            }
            // The one place the "`_` is a character, not a byte" rule lives.
            SegItem::Skip(n) => at = skip_chars(s, at, *n)?,
        }
    }
    Some(at)
}

/// Byte offset of the character `width` characters from the end of `s`.
fn start_from_end(s: &str, width: usize) -> Option<usize> {
    if width == 0 {
        return Some(s.len());
    }
    Some(s.char_indices().rev().nth(width - 1)?.0)
}

/// Leftmost placement of `seg` starting at or after `from` and ending at or
/// before `end`, returning the byte offset just past it.
///
/// The residual worst case is the confirm-and-retry loop below (`%aaa…ab%`),
/// which is naive substring search — the same complexity class PG accepts.
fn find_segment<C: CaseCmp>(s: &str, from: usize, end: usize, seg: &Segment) -> Option<usize> {
    // Items alternate by construction, so a leading `Skip` is followed by a
    // `Lit` unless the segment is nothing but that `Skip`.
    let (lead, rest) = match seg.items.first() {
        Some(SegItem::Skip(n)) => (*n, &seg.items[1..]),
        _ => (0, &seg.items[..]),
    };
    // A pure run of `_` is only a length constraint; the leftmost placement is
    // the earliest one.
    let base = skip_chars(s, from, lead)?;
    let Some(SegItem::Lit(needle)) = rest.first() else {
        return (base <= end).then_some(base);
    };
    let mut search = base;
    loop {
        let at = search + C::find(s.get(search..end)?, needle)?;
        // `C::find` has already compared `needle`, so resume past it rather
        // than letting `match_at` redo that comparison. When the segment is a
        // lone literal — the `%lit%` shape — `rest[1..]` is empty and the
        // confirm cannot fail, so the retry below never runs.
        match match_at::<C>(s, at + needle.len(), &rest[1..]) {
            // Occurrences are found left to right and the segment's width is
            // fixed, so once a placement overruns `end` every later one does.
            Some(e) if e > end => return None,
            Some(e) => return Some(e),
            None => search = next_char(s, at)?,
        }
    }
}

impl LikeProgram {
    /// The variant's name, so a test can assert a pattern specialized rather
    /// than silently falling back to the general path.
    #[cfg(test)]
    fn shape(&self) -> &'static str {
        match self {
            LikeProgram::Exact(_) => "Exact",
            LikeProgram::Prefix(_) => "Prefix",
            LikeProgram::Suffix(_) => "Suffix",
            LikeProgram::Contains(_) => "Contains",
            LikeProgram::Whole(_) => "Whole",
            LikeProgram::Segments { .. } => "Segments",
        }
    }

    /// The subject bytes whose ASCII-ness decides whether folding per byte is
    /// equivalent to lowering the whole subject, or `None` when only the whole
    /// subject can answer that.
    ///
    /// `Prefix` and `Suffix` compare a bounded window and ignore everything
    /// else, and non-ASCII outside that window cannot change how the ASCII
    /// inside it lowers. The other shapes have no such window: `'K'` (U+212A)
    /// lowers into the ASCII `k` a `Contains` scan would otherwise miss, and
    /// `İ` changes how many characters `_` has to count.
    ///
    /// A subject shorter than the literal yields `None`, not `false`: lowering
    /// can *lengthen* a subject, so a short one can still match.
    fn ascii_window<'a>(&self, s: &'a str) -> Option<&'a [u8]> {
        match self {
            LikeProgram::Prefix(lit) => s.as_bytes().get(..lit.len()),
            LikeProgram::Suffix(lit) => s.as_bytes().get(s.len().checked_sub(lit.len())?..),
            _ => None,
        }
    }

    fn matches<C: CaseCmp>(&self, s: &str) -> bool {
        match self {
            LikeProgram::Exact(lit) => C::eq(s, lit),
            LikeProgram::Prefix(lit) => C::starts_with(s, lit),
            LikeProgram::Suffix(lit) => C::ends_with(s, lit),
            LikeProgram::Contains(lit) => C::find(s, lit).is_some(),
            LikeProgram::Whole(seg) => match_at::<C>(s, 0, &seg.items) == Some(s.len()),
            LikeProgram::Segments { head, mids, tail } => {
                self.match_segments::<C>(s, head.as_ref(), mids, tail.as_ref())
            }
        }
    }

    fn match_segments<C: CaseCmp>(
        &self,
        s: &str,
        head: Option<&Segment>,
        mids: &[Segment],
        tail: Option<&Segment>,
    ) -> bool {
        let Some(mut cur) = (match head {
            Some(h) => match_at::<C>(s, 0, &h.items),
            None => Some(0),
        }) else {
            return false;
        };
        let end = match tail {
            None => s.len(),
            Some(t) => {
                let Some(start) = start_from_end(s, t.width) else {
                    return false;
                };
                if match_at::<C>(s, start, &t.items) != Some(s.len()) {
                    return false;
                }
                start
            }
        };
        if end < cur {
            return false;
        }
        // Greedy leftmost placement is complete, which is what separates glob
        // from regex: every segment has a fixed width, so if some placement
        // p₁<…<p_k succeeds then the leftmost choices q_i satisfy q_i ≤ p_i by
        // induction (q_i is the earliest match at or after q_{i-1}+w_{i-1} ≤
        // p_{i-1}+w_{i-1} ≤ p_i, so p_i is inside q_i's search window).
        // Hence no backtracking: `%a%a%a%b` over `aaa…a` is a single pass.
        for m in mids {
            match find_segment::<C>(s, cur, end, m) {
                Some(next) => cur = next,
                None => return false,
            }
        }
        true
    }
}

/// Compile a LIKE pattern, honoring `escape` (which makes the next character a
/// literal). A pattern ending in a bare escape character is an error, as in PG.
///
/// For a case-insensitive program the pattern and escape are lowercased first,
/// exactly as the interpreted implementation did, so the compiled literals live
/// in "lowered" space and matching against a lowered subject is case-sensitive.
fn compile_like(pattern: &str, kind: LikeKind) -> Result<LikeProgram> {
    let lowered;
    let (pattern, escape) = if kind.case_insensitive {
        lowered = pattern.to_lowercase();
        (
            lowered.as_str(),
            kind.escape.and_then(|e| e.to_lowercase().next()),
        )
    } else {
        (pattern, kind.escape)
    };

    let mut parts: Vec<Segment> = Vec::new();
    let mut items: Vec<SegItem> = Vec::new();
    let mut lit = String::new();
    let mut width = 0usize;

    fn flush(items: &mut Vec<SegItem>, lit: &mut String) {
        if !lit.is_empty() {
            items.push(SegItem::Lit(std::mem::take(lit)));
        }
    }

    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        // The escape check must precede `%`/`_`: `ESCAPE '%'` is legal.
        if Some(c) == escape {
            match chars.next() {
                Some(next) => {
                    lit.push(next);
                    width += 1;
                }
                None => {
                    return Err(TextError::new(
                        sqlstate::INVALID_ESCAPE_SEQUENCE,
                        "LIKE pattern must not end with escape character",
                    ));
                }
            }
        } else if c == '%' {
            flush(&mut items, &mut lit);
            parts.push(Segment {
                items: std::mem::take(&mut items),
                width,
            });
            width = 0;
        } else if c == '_' {
            flush(&mut items, &mut lit);
            match items.last_mut() {
                Some(SegItem::Skip(n)) => *n += 1,
                _ => items.push(SegItem::Skip(1)),
            }
            width += 1;
        } else {
            lit.push(c);
            width += 1;
        }
    }
    flush(&mut items, &mut lit);
    parts.push(Segment { items, width });

    Ok(narrow(parts))
}

/// Turn the parsed runs into the narrowest program that expresses them.
fn narrow(mut parts: Vec<Segment>) -> LikeProgram {
    // No `%` at all: the single run must cover the whole subject.
    if parts.len() == 1 {
        let whole = parts.pop().expect("one part");
        return match whole.as_lit() {
            Some(lit) => LikeProgram::Exact(lit.to_string()),
            None if whole.is_empty() => LikeProgram::Exact(String::new()),
            None => LikeProgram::Whole(whole),
        };
    }
    let tail = parts.pop().expect("at least two parts");
    let mut rest = parts.into_iter();
    let head = rest.next().expect("at least two parts");
    // Empty middle runs come from `%%` and constrain nothing.
    let mids: Vec<Segment> = rest.filter(|m| !m.is_empty()).collect();

    let head = (!head.is_empty()).then_some(head);
    let tail = (!tail.is_empty()).then_some(tail);
    match (&head, mids.as_slice(), &tail) {
        (None, [], None) => LikeProgram::Contains(String::new()),
        (None, [only], None) => match only.as_lit() {
            Some(lit) => LikeProgram::Contains(lit.to_string()),
            None => LikeProgram::Segments { head, mids, tail },
        },
        (Some(h), [], None) => match h.as_lit() {
            Some(lit) => LikeProgram::Prefix(lit.to_string()),
            None => LikeProgram::Segments { head, mids, tail },
        },
        (None, [], Some(t)) => match t.as_lit() {
            Some(lit) => LikeProgram::Suffix(lit.to_string()),
            None => LikeProgram::Segments { head, mids, tail },
        },
        _ => LikeProgram::Segments { head, mids, tail },
    }
}

thread_local! {
    /// Compiled LIKE patterns, most-recently-used first. See [`with_cached`]
    /// for the discipline; entries are lent out by reference rather than
    /// cloned, since a program owns its literals.
    static LIKE_CACHE: std::cell::RefCell<Patterns<LikeKind, LikeProgram>> =
        const { std::cell::RefCell::new(Patterns::new()) };
}

/// `LIKE` (and `ILIKE` when `case_insensitive`). `escape` defaults to `\` at
/// the call site; pass `None` here to disable escaping (`ESCAPE ''`).
pub fn like(s: &str, pattern: &str, escape: Option<char>, case_insensitive: bool) -> Result<bool> {
    let kind = LikeKind {
        escape,
        case_insensitive,
    };
    with_cached(&LIKE_CACHE, pattern, kind, compile_like, |prog| {
        if !case_insensitive {
            return prog.matches::<Exact>(s);
        }
        // Scanning the whole subject to classify it costs more than a `Prefix`
        // or `Suffix` match does, so those shapes only classify the window they
        // compare; everything else has to look at all of it.
        let ascii = match prog.ascii_window(s) {
            Some(window) => window.is_ascii(),
            None => s.is_ascii(),
        };
        if ascii {
            // The pattern was lowercased at compile time, so an ASCII subject
            // only needs per-byte folding — no allocation. This is exactly
            // equal to lowering it: over ASCII, `to_lowercase` maps `A-Z` to
            // `a-z`, touches nothing else, and preserves length (its one
            // contextual rule, final sigma, needs a non-ASCII `Σ`).
            prog.matches::<AsciiFold>(s)
        } else {
            // Rust lowers with *full* contextual mapping where PostgreSQL maps
            // each character independently, so this branch is wrong in both
            // directions: `İ` lowers to two characters here (changing what `_`
            // counts) against PG's one, and `'ΑΣ' ILIKE 'ασ'` is true in PG but
            // false here because of the final-sigma rule.
            //
            // TODO: PostgreSQL's simple (per-character, 1:1) case mapping, and
            // the collation tailoring on top of it — `lower('İ' COLLATE
            // "tr-x-icu")` differs from `"en-x-icu"`, and neither is reachable
            // while `like` takes no collation. Fixing it means a simple-mapping
            // table (`icu_casemap`) shared with `lower`/`upper`/`initcap`, plus
            // a collation OID threaded through `ScalarFn::{Lower,Upper,Initcap}`
            // and `text::like`.
            prog.matches::<Exact>(&s.to_lowercase())
        }
    })
}

/// How many compiled LIKE patterns this thread is holding.
#[cfg(test)]
fn like_cache_len() -> usize {
    LIKE_CACHE.with(|c| c.borrow().entries.len())
}

/// This thread's consecutive-miss count for the LIKE cache.
#[cfg(test)]
fn like_cache_misses() -> u32 {
    LIKE_CACHE.with(|c| c.borrow().misses)
}

#[cfg(test)]
enum LikeTok {
    Any,       // %
    One,       // _
    Lit(char), // an ordinary (or escaped) literal character
}

/// The interpreted matcher `compile_like` replaced, kept as the oracle the
/// compiled one is differentially tested against. Do not use outside tests.
#[cfg(test)]
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

#[cfg(test)]
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

/// The oracle's entry point: what [`like`] did before it compiled patterns.
#[cfg(test)]
fn reference_like(
    s: &str,
    pattern: &str,
    escape: Option<char>,
    case_insensitive: bool,
) -> Result<bool> {
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

/// Turn a `regex` crate compile failure into an error.
///
/// Two different things can go wrong, and PG distinguishes them by outcome even
/// though the crate does not: the pattern may be *malformed*, which PG also
/// rejects (`2201B`, matching its `invalid regular expression: ...`), or it may
/// be perfectly valid POSIX that this engine cannot execute — a backreference
/// or a look-around, both of which PG supports. Reporting the second kind as a
/// syntax error would be a lie, and silently treating it as "no match" would
/// return wrong rows, so it is `0A000`.
///
/// `regex::Error` is opaque (`Syntax(String)`), so the pattern is re-parsed
/// with `regex_syntax` to classify it. That only happens on the error path.
fn invalid_regex(e: regex::Error, source: &str, opts: ReOpts) -> TextError {
    use regex_syntax::ast::ErrorKind;

    let unsupported = matches!(
        regex_syntax::ParserBuilder::new()
            .case_insensitive(opts.case_insensitive)
            .multi_line(opts.multi_line)
            .dot_matches_new_line(opts.dot_all)
            .build()
            .parse(source),
        Err(regex_syntax::Error::Parse(ref e))
            if matches!(
                e.kind(),
                ErrorKind::UnsupportedBackreference | ErrorKind::UnsupportedLookAround
            )
    );
    if unsupported {
        return TextError::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "regular expression backreferences and look-around are not supported",
        );
    }
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
            // `t` ("tight") is the inverse of `x`, and the default.
            't' => opts.ignore_whitespace = false,
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
            // `b` and `e` select a BRE and an ERE, grammars in which `+`, `(`
            // and the `\d`-style shorthands mean something else than they do in
            // the ARE this engine speaks. Accepting either as a no-op would
            // silently return wrong rows, so both are refused outright.
            'b' => {
                return Err(TextError::new(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "basic regular expressions (flag \"b\") are not supported",
                ));
            }
            'e' => {
                return Err(TextError::new(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "extended regular expressions (flag \"e\") are not supported",
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
    // A literal pattern has no syntax to expand or to anchor, so PG refuses `q`
    // alongside expanded mode or any of the newline modes. `s` is the default
    // and `i` still applies, so neither of those is a conflict.
    if opts.literal && (opts.ignore_whitespace || opts.multi_line || !opts.dot_all) {
        return Err(TextError::new(
            sqlstate::INVALID_REGULAR_EXPRESSION,
            "invalid regular expression: invalid argument to regex function",
        ));
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
fn rewrite_pattern(pattern: &str, opts: ReOpts) -> Result<String> {
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
                // A POSIX pattern has no SIMILAR escape character; the
                // backslash handling built into `take_bracket` is enough.
                let (class, closed) = take_bracket(&mut chars, None);
                if !closed {
                    // Hand the imbalance to the compiler rather than inventing a
                    // terminator, so a malformed class stays an error.
                    out.push('[');
                    out.push_str(&class);
                } else if no_newline && class.starts_with('^') {
                    // Negated, and `class` already carries its own leading `^`.
                    out.push_str("[[");
                    out.push_str(&class);
                    out.push_str("]&&[^\\n]]");
                } else {
                    out.push('[');
                    out.push_str(&class);
                    out.push(']');
                }
            }
            // Expanded mode ignores whitespace, but not *within* a
            // multi-character symbol: PG rejects `( ?:` rather than reading it
            // as `(?:`. Only `(?` is checked; other multi-character symbols
            // cannot be split by whitespace in this dialect.
            '(' if expand => {
                out.push('(');
                let mut ahead = chars.clone();
                let mut spaced = false;
                while ahead.peek().is_some_and(|c| c.is_whitespace()) {
                    ahead.next();
                    spaced = true;
                }
                if ahead.peek() == Some(&'?') {
                    if spaced {
                        return Err(TextError::new(
                            sqlstate::INVALID_REGULAR_EXPRESSION,
                            "invalid regular expression: quantifier operand invalid",
                        ));
                    }
                    // Copy `(?` and the character that completes the symbol
                    // without giving whitespace a chance to be stripped.
                    chars.next();
                    out.push('?');
                    match chars.next() {
                        Some(c) if c.is_whitespace() => {
                            return Err(TextError::new(
                                sqlstate::INVALID_REGULAR_EXPRESSION,
                                "invalid regular expression: quantifier operand invalid",
                            ));
                        }
                        Some(c) => out.push(c),
                        None => {}
                    }
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
    Ok(out)
}

/// Consume a bracket expression's body from `chars`, which is positioned just
/// after the opening `[`, and return it without the enclosing brackets together
/// with whether the closing `]` was actually found.
///
/// Handles the POSIX rules that make `]` a literal member: a leading `^` and a
/// `]` in first position, plus `[:name:]`/`[.x.]`/`[=x=]` sub-expressions and a
/// backslash escape. A caller must **not** supply a closing `]` of its own when
/// `closed` is false: leaving the class unbalanced is what makes the regex
/// compiler report PG's `brackets [] not balanced`, instead of silently turning
/// a malformed pattern into a valid one that matches.
fn take_bracket(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    escape: Option<char>,
) -> (String, bool) {
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
        // The SIMILAR escape character carries its next character too, so an
        // escaped `]` is a class member rather than the end of the class.
        if Some(c) == escape {
            body.push(c);
            if let Some(next) = chars.next() {
                body.push(next);
            }
            continue;
        }
        match c {
            ']' => return (body, true),
            // `[:alpha:]` and friends: copy through to the matching delimiter so
            // the inner `]` does not close the outer class.
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
    (body, false)
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

/// How many compiled patterns to keep per thread, per cache. The bound exists
/// to cap per-thread memory for a cache consulted once per row; the exact depth
/// is not observable, so any small number would do.
const PATTERN_CACHE_MAX: usize = 32;

/// How often a thrashing cache still records an entry. Without a periodic probe
/// a cache that has stopped recording could never register a hit, so it could
/// never recover once a pattern did become hot.
const PATTERN_CACHE_PROBE: u32 = 16;

/// Patterns compiled to `V`, keyed on the user's pattern text plus a `K`
/// describing how to read it.
struct Patterns<K, V> {
    /// Most-recently-used first.
    entries: Vec<(String, K, V)>,
    /// Consecutive misses. A pattern that varies per row — `a LIKE b`, or a
    /// computed `a LIKE '%' || b || '%'` — has a hit rate of zero, and paying
    /// the key copy, the memmove to the front and the evicted entry's drop on
    /// every row is slower than never caching at all.
    misses: u32,
}

/// A thread-local [`Patterns`].
type PatternCache<K, V> = std::thread::LocalKey<std::cell::RefCell<Patterns<K, V>>>;

impl<K, V> Patterns<K, V> {
    const fn new() -> Self {
        Patterns {
            entries: Vec::new(),
            misses: 0,
        }
    }
}

thread_local! {
    /// Most-recently-used first. Cloning a `Regex` is *not* free — it allocates
    /// a fresh, empty match-state pool — so entries are lent out by reference
    /// (see [`with_cached`]) rather than cloned per row.
    static RE_CACHE: std::cell::RefCell<Patterns<PatternKind, regex::Regex>> =
        const { std::cell::RefCell::new(Patterns::new()) };
}

/// Compile `pattern` according to `kind` (reusing a live cache entry when there
/// is one) and run `f` against it. Shared by the regex/`SIMILAR TO` cache and
/// the `LIKE` one; they differ only in what a compiled pattern *is*.
///
/// `f` must not itself call back into the cache: the entry is lent out while
/// the thread-local is borrowed, so re-entering would panic. Every caller in
/// this module runs a single match and returns an owned result.
///
/// A failed compile is never inserted, so a bad pattern misses on every row and
/// re-raises its error each time — which is what PG does, and what a future
/// "cache the `Err` too" would silently break.
fn with_cached<K, V, T>(
    cache: &'static PatternCache<K, V>,
    pattern: &str,
    kind: K,
    compile: impl FnOnce(&str, K) -> Result<V>,
    f: impl FnOnce(&V) -> T,
) -> Result<T>
where
    K: Copy + PartialEq + 'static,
    V: 'static,
{
    cache.with(|cache| {
        let cache = &mut *cache.borrow_mut();
        // More consecutive misses than the cache could ever hold means the
        // pattern varies per row rather than being merely cold, and everything
        // the cache does is then pure overhead. Both halves shrink: only the
        // most recent entry is consulted, and a new one is recorded once every
        // `PATTERN_CACHE_PROBE` misses — often enough that a pattern which does
        // turn hot lands in slot 0 and starts hitting, which zeroes the counter
        // and restores the full cache.
        let thrashing = cache.misses > PATTERN_CACHE_MAX as u32;
        let scan = if thrashing {
            1.min(cache.entries.len())
        } else {
            cache.entries.len()
        };
        match cache.entries[..scan]
            .iter()
            .position(|(p, k, _)| *k == kind && p == pattern)
        {
            Some(idx) => {
                cache.misses = 0;
                // Promote to most-recently-used. Already-hot patterns (the
                // common case for a per-row scan) need no shuffling at all.
                if idx != 0 {
                    cache.entries[..=idx].rotate_right(1);
                }
            }
            None => {
                let compiled = compile(pattern, kind)?;
                cache.misses = cache.misses.saturating_add(1);
                if thrashing && !cache.misses.is_multiple_of(PATTERN_CACHE_PROBE) {
                    return Ok(f(&compiled));
                }
                cache
                    .entries
                    .insert(0, (pattern.to_string(), kind, compiled));
                cache.entries.truncate(PATTERN_CACHE_MAX);
            }
        }
        Ok(f(&cache.entries[0].2))
    })
}

/// Build the `Regex` behind a [`PatternKind`].
fn compile_regex(pattern: &str, kind: PatternKind) -> Result<regex::Regex> {
    // The `q` flag makes the whole pattern a literal string; otherwise apply
    // the rewrites the crate cannot express.
    let (opts, source) = match kind {
        PatternKind::SimilarTo(escape) => (ReOpts::default(), similar_to_regex(pattern, escape)?),
        PatternKind::Regex(opts) if opts.literal => (opts, regex::escape(pattern)),
        PatternKind::Regex(opts) => (opts, rewrite_pattern(pattern, opts)?),
    };
    regex::RegexBuilder::new(&source)
        .case_insensitive(opts.case_insensitive)
        .multi_line(opts.multi_line)
        .dot_matches_new_line(opts.dot_all)
        .build()
        .map_err(|e| invalid_regex(e, &source, opts))
}

/// POSIX regex match, backing the `~` (case-sensitive) and `~*`
/// (case-insensitive) operators. The match is *unanchored*: `~` succeeds when
/// the pattern matches anywhere in `s`, as in PG.
pub fn regex_match(s: &str, pattern: &str, case_insensitive: bool) -> Result<bool> {
    let opts = ReOpts {
        case_insensitive,
        ..ReOpts::default()
    };
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::Regex(opts),
        compile_regex,
        |re| re.is_match(s),
    )
}

/// The flag set jsonpath's `like_regex ... flag "..."` accepts. It is
/// XQuery-flavored rather than POSIX, so it is both a different set and a
/// different default from [`parse_re_flags`]: `.` does *not* span a newline
/// unless `s` asks for it.
///
/// Held as a parsed set rather than the literal text because PG re-emits it in
/// a fixed order with duplicates collapsed, so `flag "qmi"` prints as
/// `flag "imq"`.
#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LikeRegexFlags {
    /// `i` — case-insensitive.
    pub icase: bool,
    /// `s` — `.` matches a newline.
    pub dotall: bool,
    /// `m` — `^`/`$` match at line boundaries.
    pub mline: bool,
    /// `x` — expanded mode. PG rejects this flag unless `q` is also present,
    /// and `q` makes the pattern literal, so it never affects a match.
    pub wspace: bool,
    /// `q` — the pattern is a literal string.
    pub quote: bool,
}

impl LikeRegexFlags {
    /// Parse a flag string, reporting the first unrecognized character.
    pub fn parse(flags: &str) -> std::result::Result<Self, char> {
        let mut out = LikeRegexFlags::default();
        for c in flags.chars() {
            match c {
                'i' => out.icase = true,
                's' => out.dotall = true,
                'm' => out.mline = true,
                'x' => out.wspace = true,
                'q' => out.quote = true,
                other => return Err(other),
            }
        }
        Ok(out)
    }

    pub fn is_empty(self) -> bool {
        self == LikeRegexFlags::default()
    }

    /// PG's spelling of the set: the flags it contains, in a fixed order.
    pub fn canonical(self) -> String {
        let mut out = String::new();
        for (on, c) in [
            (self.icase, 'i'),
            (self.dotall, 's'),
            (self.mline, 'm'),
            (self.wspace, 'x'),
            (self.quote, 'q'),
        ] {
            if on {
                out.push(c);
            }
        }
        out
    }

    fn opts(self) -> ReOpts {
        ReOpts {
            case_insensitive: self.icase,
            multi_line: self.mline,
            dot_all: self.dotall,
            // Never set: `x` only survives parsing alongside `q`, which escapes
            // the whole pattern, leaving expanded mode unobservable.
            ignore_whitespace: false,
            literal: self.quote,
        }
    }
}

/// jsonpath's `like_regex`.
pub fn like_regex_match(s: &str, pattern: &str, flags: LikeRegexFlags) -> Result<bool> {
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::Regex(flags.opts()),
        compile_regex,
        |re| re.is_match(s),
    )
}

/// Compile-check a `like_regex` pattern without matching anything, as PG does
/// while *parsing* the path — a bad pattern is an error on the cast, not on the
/// row. The compile is not wasted work: it seats the pattern at the head of the
/// per-thread cache, where the first evaluation finds it.
pub fn like_regex_compile(pattern: &str, flags: LikeRegexFlags) -> Result<()> {
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::Regex(flags.opts()),
        compile_regex,
        |_| (),
    )
}

/// `SIMILAR TO`: an SQL-standard pattern language distinct from both LIKE and
/// POSIX regex. It is case-sensitive and matches the *whole* string (unlike
/// `~`). We translate it to a POSIX regex and delegate to the `regex` crate.
pub fn similar_to_match(s: &str, pattern: &str, escape: Option<char>) -> Result<bool> {
    // The cache is keyed on the SIMILAR TO pattern itself, so a repeated row
    // skips the translation as well as the compile.
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::SimilarTo(escape),
        compile_regex,
        |re| re.is_match(s),
    )
}

/// The value the two-and-three-argument `substring` forms extract from a match:
/// the first capture group when the pattern has one, otherwise the whole match.
/// A group that did not participate yields NULL rather than the empty string.
fn extracted(re: &regex::Regex, s: &str) -> Option<String> {
    // Asking for captures allocates a slot buffer per call, so a pattern with no
    // subexpression — the common `substring(col from '...')` — takes `find`
    // instead, like `regexp_count` and `regexp_like` next door.
    if re.captures_len() == 1 {
        return re.find(s).map(|m| m.as_str().to_string());
    }
    re.captures(s)?.get(1).map(|m| m.as_str().to_string())
}

/// `substring(string, pattern)`: POSIX-regex extraction. Returns the pattern's
/// first parenthesised subexpression, or the whole match when it has none, and
/// `None` (SQL NULL) when the pattern does not match at all.
///
/// **Documented divergence.** PG's engine is POSIX leftmost-*longest* while the
/// `regex` crate is Perl leftmost-*first*, so an alternation whose later branch
/// is longer extracts less than PG does: `substring('foobar' from 'o|oo')` is
/// `oo` in PG and `o` here. The whole `~`/`regexp_*` family shares the trait,
/// but this is the first function where it changes a returned *value* rather
/// than a boolean. Fixing it means replacing the engine, not the translation —
/// reordering a user's alternation branches is not sound.
pub fn substring_regex(s: &str, pattern: &str) -> Result<Option<String>> {
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::Regex(ReOpts::default()),
        compile_regex,
        |re| extracted(re, s),
    )
}

/// `substring(string, pattern, escape)`: the SQL-regex form, spelled
/// `SUBSTRING(s SIMILAR pat ESCAPE e)` or `SUBSTRING(s FROM pat FOR e)`. The
/// pattern is a `SIMILAR TO` pattern whose escape-double-quote separators mark
/// the part to extract (see [`similar_to_regex`]); with no separators the whole
/// match is returned.
pub fn substring_similar(s: &str, pattern: &str, escape: Option<char>) -> Result<Option<String>> {
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::SimilarTo(escape),
        compile_regex,
        |re| extracted(re, s),
    )
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

thread_local! {
    /// The last replacement string and its translation. The replacement is a
    /// constant literal in nearly every query, so this keeps the rewrite off
    /// the per-row path the way [`RE_CACHE`] keeps compilation off it. One slot
    /// is enough: a query has one replacement.
    static LAST_REPLACEMENT: std::cell::RefCell<(String, String)> =
        const { std::cell::RefCell::new((String::new(), String::new())) };
}

fn translate_replacement_cached(replacement: &str) -> String {
    LAST_REPLACEMENT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.0 != replacement {
            *slot = (replacement.to_string(), translate_replacement(replacement));
        }
        slot.1.clone()
    })
}

/// `regexp_replace(source, pattern, replacement [, flags])`. Without the `g`
/// flag only the first match is replaced.
pub fn regexp_replace(s: &str, pattern: &str, replacement: &str, flags: &str) -> Result<String> {
    regexp_replace_at(s, pattern, replacement, 1, None, flags)
}

/// `regexp_replace(source, pattern, replacement, start [, n [, flags]])`.
///
/// `n` is `None` for the flags-only form, where `g` chooses between the first
/// match and all of them. When it is given it wins over `g`: `0` means every
/// match at or after `start`, and `k` means only the `k`th.
pub fn regexp_replace_at(
    s: &str,
    pattern: &str,
    replacement: &str,
    start: i32,
    n: Option<i32>,
    flags: &str,
) -> Result<String> {
    let offset = start_offset(s, start)?;
    if let Some(n) = n
        && n < 0
    {
        return Err(invalid_parameter("n", n));
    }
    let (opts, global) = parse_re_flags(flags)?;
    let replacement = translate_replacement_cached(replacement);
    // A `start` past the end of the string leaves it untouched.
    let Some(offset) = offset else {
        return Ok(s.to_string());
    };
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::Regex(opts),
        compile_regex,
        |re| {
            // Walking the matches explicitly rather than calling `replace_all`,
            // which suppresses a zero-width match sitting where the previous match
            // ended; PG replaces it.
            let mut out = String::with_capacity(s.len());
            let mut copied = 0;
            let mut cursor = offset;
            let mut seen = 0;
            while let Some(caps) = re.captures_at(s, cursor) {
                let m = caps.get(0).expect("group 0 always participates");
                seen += 1;
                if n.is_none_or(|k| k == 0 || k == seen) {
                    out.push_str(&s[copied..m.start()]);
                    caps.expand(&replacement, &mut out);
                    copied = m.end();
                }
                let last = match n {
                    Some(0) => false,
                    Some(k) => seen >= k,
                    None => !global,
                };
                cursor = advance(s, &m);
                if last || cursor > s.len() {
                    break;
                }
            }
            out.push_str(&s[copied..]);
            out
        },
    )
}

/// `regexp_like(string, pattern [, flags])` — the functional spelling of `~`.
pub fn regexp_like(s: &str, pattern: &str, flags: &str) -> Result<bool> {
    let (opts, global) = parse_re_flags(flags)?;
    if global {
        return Err(reject_global("regexp_like"));
    }
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::Regex(opts),
        compile_regex,
        |re| re.is_match(s),
    )
}

/// Where the scan resumes after `m`. Emptiness is decided by the *match*, not
/// by where the search started: an empty match found ahead of the cursor would
/// otherwise leave the cursor sitting on it and be found a second time. A
/// zero-width match advances by a whole character so the scan cannot stall.
fn advance(s: &str, m: &regex::Match<'_>) -> usize {
    if m.end() > m.start() {
        return m.end();
    }
    let end = m.end();
    s[end..]
        .chars()
        .next()
        .map_or(end + 1, |c| end + c.len_utf8())
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
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::Regex(opts),
        compile_regex,
        |re| {
            // PG re-seeds the non-overlapping scan *at* `start`, so a match that
            // began earlier is re-found clipped rather than skipped. `find_at`
            // keeps `^` and look-behind aware of the text before `start`, which
            // slicing the haystack would not.
            let mut cursor = offset;
            let mut count: i32 = 0;
            while let Some(m) = re.find_at(s, cursor) {
                count = count.saturating_add(1);
                cursor = advance(s, &m);
                if cursor > s.len() {
                    break;
                }
            }
            count
        },
    )
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
    with_cached(
        &RE_CACHE,
        pattern,
        PatternKind::Regex(opts),
        compile_regex,
        |re| {
            // Walk to the `n`th match the same way `regexp_count` counts them.
            let mut cursor = offset;
            for _ in 1..n {
                let m = re.find_at(s, cursor)?;
                cursor = advance(s, &m);
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
        },
    )
}

/// Which match preference a `SIMILAR TO` segment must have as a whole.
///
/// PG expresses this by wrapping a segment in `{1,1}?` (prefer shortest) or
/// `{1,1}` (prefer longest), which in its engine flips the preference of the
/// *entire* wrapped subexpression. That cannot be ported literally: in the
/// `regex` crate the `?` applies only to the repetition count — already fixed at
/// one — so `^(?:.*){1,1}?(.*)$` still lets the inner `.*` run greedily and
/// captures nothing. The preference has to be pushed down onto every quantifier
/// the segment emits instead, which is what [`push_quantifier`] does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Greed {
    /// Keep whatever the user wrote: the suffix segment, and a pattern with no
    /// separators at all.
    AsIs,
    /// The segment before the first separator: prefer the shortest match, so
    /// extraction starts as early as possible.
    Shortest,
    /// The extracted segment: prefer the longest match, overriding any lazy
    /// marker the user wrote inside it.
    Longest,
}

/// Emit one quantifier token under `greed`.
///
/// The user's own lazy marker is *consumed* before the segment's preference is
/// applied, so at most one `?` is ever emitted per token. Appending blindly
/// would build `a*??` from the user's `a*?` and `a*???` from `a*??`, and the
/// regex crate is not a reliable backstop for that — it rejects `a**` but
/// accepts both `.*+` and `.*?+`.
fn push_quantifier(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    out: &mut String,
    token: &str,
    greed: Greed,
) {
    let lazy = chars.peek() == Some(&'?');
    if lazy {
        chars.next();
    }
    out.push_str(token);
    match greed {
        Greed::Shortest => out.push('?'),
        Greed::Longest => {}
        Greed::AsIs if lazy => out.push('?'),
        Greed::AsIs => {}
    }
}

/// Read the next `count` hexadecimal digits, or as many as there are when
/// `count` is `None`. PG's `\u`/`\U` want a fixed width; `\x` takes what it can.
fn take_hex(chars: &mut std::iter::Peekable<std::str::Chars>, count: Option<usize>) -> Option<u32> {
    let mut digits = String::new();
    while count.is_none_or(|n| digits.len() < n) {
        match chars.peek() {
            Some(c) if c.is_ascii_hexdigit() => {
                digits.push(*c);
                chars.next();
            }
            _ => break,
        }
    }
    if digits.is_empty() || count.is_some_and(|n| digits.len() != n) {
        return None;
    }
    u32::from_str_radix(&digits, 16).ok()
}

/// Emit a code point as `\x{..}`, which the regex crate accepts both on its own
/// and inside a bracket expression.
fn push_code_point(out: &mut String, value: u32) {
    out.push_str(&format!("\\x{{{value:X}}}"));
}

fn invalid_escape() -> TextError {
    TextError::new(
        sqlstate::INVALID_REGULAR_EXPRESSION,
        "invalid regular expression: invalid escape \\ sequence",
    )
}

/// Translate the character(s) following the escape character into regex source.
///
/// PG does not make the escaped character a literal: it re-emits it as an ARE
/// escape (`similar_to_escape('#d', '#')` is `^(?:\d)$`), so `escape` + `d` is
/// the digit class and `escape` + `q` is an error, not the letters `d` and `q`.
/// The spellings do not line up one-for-one with the regex crate, so each is
/// mapped explicitly rather than passed through: PG's `\b` is a backspace where
/// the crate reads a word boundary, and its `\B` is a literal backslash where
/// the crate reads a *non*-boundary — passing those through would silently
/// invert them.
fn push_are_escape(
    out: &mut String,
    chars: &mut std::iter::Peekable<std::str::Chars>,
    c: char,
    in_class: bool,
) -> Result<()> {
    match c {
        // Character escapes. Spelled as code points so a crate-side difference
        // in the letter escapes cannot change their meaning.
        'a' => push_code_point(out, 0x07),
        'b' => push_code_point(out, 0x08),
        'e' => push_code_point(out, 0x1B),
        'f' => push_code_point(out, 0x0C),
        'n' => push_code_point(out, 0x0A),
        'r' => push_code_point(out, 0x0D),
        't' => push_code_point(out, 0x09),
        'v' => push_code_point(out, 0x0B),
        // `\B` is PG's spelling of a literal backslash.
        'B' => out.push_str("\\\\"),
        // Class shorthands, which mean the same in both engines and are legal
        // inside a bracket expression.
        'd' | 'D' | 's' | 'S' | 'w' | 'W' => {
            out.push('\\');
            out.push(c);
        }
        // Zero-width constraints. PG rejects these inside a bracket expression.
        'y' | 'Y' | 'm' | 'M' | 'A' | 'Z' if in_class => return Err(invalid_escape()),
        'y' => out.push_str("\\b"),
        'Y' => out.push_str("\\B"),
        'm' => out.push_str("\\b{start}"),
        'M' => out.push_str("\\b{end}"),
        'A' => out.push_str("\\A"),
        'Z' => out.push_str("\\z"),
        // `\cX` is the low five bits of the next character.
        'c' => match chars.next() {
            Some(x) => push_code_point(out, x as u32 & 0x1F),
            None => return Err(invalid_escape()),
        },
        'u' => push_code_point(out, take_hex(chars, Some(4)).ok_or_else(invalid_escape)?),
        'U' => push_code_point(out, take_hex(chars, Some(8)).ok_or_else(invalid_escape)?),
        'x' => push_code_point(out, take_hex(chars, None).ok_or_else(invalid_escape)?),
        // `\0` opens an octal escape of up to three digits in total.
        '0' => {
            let mut value = 0u32;
            for _ in 0..2 {
                match chars.peek() {
                    Some(d @ '0'..='7') => {
                        value = value * 8 + (*d as u32 - '0' as u32);
                        chars.next();
                    }
                    _ => break,
                }
            }
            push_code_point(out, value);
        }
        // A backreference. PG resolves these against its own groups; ours are
        // all non-capturing, and the crate has no backreferences either.
        '1'..='9' => {
            return Err(TextError::new(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "regular expression backreferences and look-around are not supported",
            ));
        }
        // Any other letter or digit is undefined, as in PG.
        other if other.is_alphanumeric() => return Err(invalid_escape()),
        // Backslash before punctuation is just that character.
        other => push_literal(out, other),
    }
    Ok(())
}

/// Emit `c` as a regex literal, escaping it when it is a regex metacharacter
/// (`regex::escape` covers the full set: `. + * ? ( ) | [ ] { } ^ $ \`).
fn push_literal(out: &mut String, c: char) {
    // `regex::escape` is the authoritative metacharacter set, so a literal char
    // stays a literal even as the regex grammar grows.
    let mut buf = [0u8; 4];
    out.push_str(&regex::escape(c.encode_utf8(&mut buf)));
}

/// Copy a bracket expression `[...]` from `chars` (positioned just after the
/// opening `[`) into `out`. Inside a bracket expression the SIMILAR TO wildcards
/// `%`/`_` lose their meaning — PG hands the contents to its regex engine as a
/// POSIX character class — so they pass through unchanged. The escape character
/// keeps working, though: `[a#"b]` under `ESCAPE '#'` is the class `{a, ", b}`
/// and does *not* contain `#`. A leading `^` and a `]` in first position are
/// literal members, and `[:name:]`/`[.x.]`/`[=x=]` sub-expressions are copied
/// whole. An unterminated class is left unbalanced so the regex compiler rejects
/// it, matching PG's `brackets [] not balanced`.
fn copy_bracket(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    out: &mut String,
    escape: Option<char>,
) -> Result<()> {
    let (body, closed) = take_bracket(chars, escape);
    out.push('[');
    let mut members = body.chars().peekable();
    while let Some(c) = members.next() {
        if Some(c) == escape {
            match members.next() {
                Some(next) => push_are_escape(out, &mut members, next, true)?,
                None => break,
            }
        } else {
            out.push(c);
        }
    }
    if closed {
        out.push(']');
    }
    Ok(())
}

/// If a valid regex bound (`{m}`, `{m,}`, `{m,n}`) follows the just-consumed
/// `{`, copy it through the closing `}` and return; otherwise emit a literal
/// `{`, leaving the following characters for the caller. PG treats a `{` that
/// does not open a bound as an ordinary character.
fn push_brace(chars: &mut std::iter::Peekable<std::str::Chars>, out: &mut String, greed: Greed) {
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
        *chars = look;
        push_quantifier(chars, out, &format!("{{{bound}}}"), greed);
    } else {
        // Digits followed `{`, so PG is already parsing a bound and reports
        // `invalid repetition count(s)` when it is never closed. Emitting the
        // `{` unescaped hands the same verdict to the regex compiler, instead of
        // quietly demoting a malformed bound to a literal.
        out.push('{');
    }
}

/// Split a `SIMILAR TO` pattern at its *escape-double-quote separators*
/// (`escape` + `"`), returning one, two or three segments of raw pattern text.
///
/// The separators mark the part of the match that `substring(text, text, text)`
/// extracts. They are structural: PG splits the pattern here and wraps each
/// piece on its own, rather than dropping a paren in place, which is why an
/// alternation never binds across a separator. A separator inside a bracket
/// expression is an ordinary class member — PG agrees, so classes are skipped
/// whole using the same [`take_bracket`] the translation uses, keeping the two
/// scans in step.
fn split_separators(pattern: &str, escape: Option<char>) -> Result<Vec<String>> {
    let mut segments = vec![String::new()];
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            match chars.next() {
                Some('"') => {
                    if segments.len() == 3 {
                        return Err(TextError::new(
                            sqlstate::INVALID_USE_OF_ESCAPE_CHARACTER,
                            "SQL regular expression may not contain more than two escape-double-quote separators",
                        ));
                    }
                    segments.push(String::new());
                }
                // Not a separator: keep the escape sequence intact for the
                // per-segment translation to interpret.
                Some(next) => {
                    let segment = segments.last_mut().expect("at least one segment");
                    segment.push(c);
                    segment.push(next);
                }
                // PG silently drops an escape character with nothing left to
                // escape: `similar_to_escape('abc\', '\')` is `^(?:abc)$`.
                None => break,
            }
        } else {
            let segment = segments.last_mut().expect("at least one segment");
            segment.push(c);
            if c == '[' {
                let (body, closed) = take_bracket(&mut chars, escape);
                segment.push_str(&body);
                if closed {
                    segment.push(']');
                }
            }
        }
    }
    Ok(segments)
}

/// Translate one separator-free segment of a `SIMILAR TO` pattern into regex
/// source, under the match preference `greed` (see [`Greed`]).
///
/// `%` becomes `.*` and `_` becomes `.` (both match any character, including a
/// newline, which the caller's `dot_all` option provides); the SQL-regex
/// metacharacters `| * + ? ( )` and valid `{...}` bounds pass through; bracket
/// expressions `[...]` are copied verbatim (see [`copy_bracket`]); every other
/// character is emitted as a regex literal. The escape character (default `\`)
/// makes the following character a literal. A parenthesised group written by the
/// user becomes the non-capturing `(?:`, so that the separator group added by
/// [`similar_to_regex`] is always group 1.
fn translate_segment(segment: &str, escape: Option<char>, greed: Greed) -> Result<String> {
    let mut out = String::new();
    let mut chars = segment.chars().peekable();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            match chars.next() {
                Some(next) => push_are_escape(&mut out, &mut chars, next, false)?,
                // `split_separators` has already dropped a trailing escape.
                None => break,
            }
        } else {
            match c {
                '%' => push_quantifier(&mut chars, &mut out, ".*", greed),
                '_' => out.push('.'),
                '(' => out.push_str("(?:"),
                '*' | '+' | '?' => {
                    let mut token = [0u8; 4];
                    push_quantifier(&mut chars, &mut out, c.encode_utf8(&mut token), greed);
                }
                // SQL-regex metacharacters shared with POSIX regex.
                '|' | ')' => out.push(c),
                '[' => copy_bracket(&mut chars, &mut out, escape)?,
                '{' => push_brace(&mut chars, &mut out, greed),
                other => push_literal(&mut out, other),
            }
        }
    }
    Ok(out)
}

/// Translate a `SIMILAR TO` pattern into an anchored POSIX regex string.
///
/// The pattern is first split at its escape-double-quote separators (see
/// [`split_separators`]), then each segment is translated separately and the
/// pieces are assembled as `^(?:PRE)(MID)(?:POST)$`. PG builds the same shape,
/// spelled `^(?:PRE){1,1}?(MID){1,1}(?:POST)$`; the wrappers there set each
/// segment's match preference, which we reproduce with [`Greed`] because the
/// `regex` crate has no equivalent construct.
///
/// `SIMILAR TO` itself ignores the capture group — it only asks whether the
/// whole string matches, which is why `'x' SIMILAR TO 'x\"'` is true — so one
/// translation serves both it and `substring(text, text, text)`.
fn similar_to_regex(pattern: &str, escape: Option<char>) -> Result<String> {
    let segments = split_separators(pattern, escape)?;
    // With no separator at all there is nothing to extract, so the whole pattern
    // keeps the preferences the user wrote.
    let head = if segments.len() == 1 {
        Greed::AsIs
    } else {
        Greed::Shortest
    };

    let mut out = String::from("^(?:");
    out.push_str(&translate_segment(&segments[0], escape, head)?);
    out.push(')');
    // A lone opening separator extracts everything from there to the end of the
    // match, which falls out of translating the (absent) suffix as empty.
    if let Some(extracted) = segments.get(1) {
        out.push('(');
        out.push_str(&translate_segment(extracted, escape, Greed::Longest)?);
        out.push(')');
    }
    if let Some(suffix) = segments.get(2) {
        out.push_str("(?:");
        out.push_str(&translate_segment(suffix, escape, Greed::AsIs)?);
        out.push(')');
    }
    out.push('$');
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
        assert_eq!(
            substr("abc", 2, Some(-1))
                .expect_err("a negative substring length")
                .sqlstate,
            "22011"
        );

        Ok(())
    }

    #[test]
    fn strpos_and_overlay() -> anyhow::Result<()> {
        assert_eq!(strpos("abcabc", "bc"), 2);
        assert_eq!(strpos("abc", ""), 1);
        assert_eq!(strpos("abc", "z"), 0);
        assert_eq!(overlay("Txxxxas", "hom", 2, Some(4))?, "Thomas");
        // A start of 0 (or below) is a negative-substring error, as in PG.
        assert_eq!(
            overlay("abc", "X", 0, None)
                .expect_err("an overlay start of 0 asks for a negative substring")
                .sqlstate,
            "22011"
        );

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
            pad("a", 2_000_000_000, "x", true)
                .expect_err("a pad length past MaxAllocSize")
                .sqlstate,
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
        assert_eq!(
            repeat("ab", 2_000_000_000)
                .expect_err("a repeat count that would build a string past MaxAllocSize")
                .sqlstate,
            "54000"
        );
        assert_eq!(reverse("café"), "éfac");

        Ok(())
    }

    #[test]
    fn left_right_split() -> anyhow::Result<()> {
        assert_eq!(left("abc", -1), "ab");
        assert_eq!(right("abc", -1), "bc");
        assert_eq!(split_part("a,b,c", ",", -1)?, "c");
        assert_eq!(split_part("abc", "", 1)?, "abc");
        assert_eq!(
            split_part("a", ",", 0)
                .expect_err("a split_part field number of 0")
                .sqlstate,
            "22023"
        );

        Ok(())
    }

    #[test]
    fn chr_and_ascii() {
        assert_eq!(ascii(""), 0);
        assert_eq!(
            chr(0)
                .expect_err("chr(0), which has no representable character")
                .sqlstate,
            "54000"
        );
        assert_eq!(
            chr(-1).expect_err("a negative chr argument").sqlstate,
            "22023"
        );
        assert_eq!(
            chr(1114112)
                .expect_err("a chr argument past the last code point")
                .sqlstate,
            "54000"
        );
        // A surrogate code point is "not valid", distinct from "too large".
        assert_eq!(
            chr(55296)
                .expect_err("chr of a surrogate code point")
                .message,
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
        let e = regex_match("abc", "a(", false).expect_err("the unclosed group in \"a(\"");
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
        let e = regexp_replace("abc", "b", "X", "z").expect_err("the unknown regex flag \"z\"");
        assert_eq!(e.sqlstate, "22023");
        assert_eq!(e.message, "invalid regular expression option: \"z\"");
        assert_eq!(
            regexp_replace("abc", "b", "X", "b")
                .expect_err("the BRE flag \"b\", a grammar this engine does not implement")
                .sqlstate,
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
        // the start of the whole string. `xaaa` discriminates: searching a
        // *slice* from `start` would anchor `^` at the `a` and find one.
        assert_eq!(regexp_count("xaaa", "^a", 2, "")?, 0);
        assert_eq!(regexp_substr("xabc", "^abc", 2, 1, "", 0)?, None);
        // A `start` past the end of the string simply finds nothing.
        assert_eq!(regexp_count("abc", "b", 9, "")?, 0);
        // An empty match must advance the cursor, not stall it — and a
        // zero-width match found *ahead* of the cursor must not be re-found.
        assert_eq!(regexp_count("abc", "", 1, "")?, 4);
        assert_eq!(regexp_count("abc", "$", 1, "")?, 1);
        assert_eq!(regexp_count("xax", "a|$", 1, "")?, 2);
        assert_eq!(regexp_substr("abc", "$", 1, 2, "", 0)?, None);
        // Multi-byte input: `start` counts characters, and the empty-match step
        // has to move a whole character or it would split a codepoint.
        assert_eq!(
            regexp_substr("äöü", ".", 2, 1, "", 0)?.as_deref(),
            Some("ö")
        );
        assert_eq!(regexp_count("äöü", "", 1, "")?, 4);
        assert_eq!(regexp_count("äöüä", "ä", 2, "")?, 1);

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

    /// A malformed bracket expression must stay malformed. Rewriting the
    /// pattern must not invent the `]` the user left out, or an invalid regex
    /// silently becomes a valid one that matches.
    #[test]
    fn unterminated_bracket_is_still_an_error() {
        for pattern in ["[abc", "a[bc", "[^abc", "[a-", "[[:alpha:]", "x[.", "[]"] {
            let Err(e) = regex_match("abc", pattern, false) else {
                panic!("{pattern:?} should not compile");
            };
            assert_eq!(e.sqlstate, "2201B", "for {pattern:?}");
        }
        // A newline-sensitive flag takes the negated-class path, which must not
        // wrap an unterminated class either.
        assert!(regexp_like("abc", "[^abc", "n").is_err());
        // The balanced forms still work.
        assert!(regex_match("abc", "[abc]", false).is_ok());
        assert!(regex_match("a]c", "[]a]", false).is_ok());
    }

    #[test]
    fn bracket_walk_branches() -> anyhow::Result<()> {
        // POSIX class, in-class escape, leading `]`, and a negated class under
        // a newline-sensitive flag (which goes through the wrap).
        assert!(regexp_like("abc", "[[:alpha:]]+", "")?);
        assert!(regexp_like("a]b", "[\\]]", "")?);
        assert!(regexp_like("]", "[]]", "")?);
        assert!(!regexp_like("\n", "[^[:alpha:]]", "n")?);
        assert!(regexp_like("\n", "[^[:alpha:]]", "")?);
        assert!(regexp_like("1", "[^[:alpha:]]", "n")?);
        assert!(!regexp_like("x", "[^[:alpha:]]", "n")?);
        // `#` is only a comment in expanded mode, where `#b` drops off and the
        // pattern is just `a`; without `x` it has to match literally.
        assert!(regexp_like("a#b", "a#b", "")?);
        assert!(regexp_like("axb", "a#b", "x")?);
        assert!(!regexp_like("axb", "a#b", "")?);
        // SIMILAR TO shares the same walk, so an in-class escape works there too.
        assert!(similar_to_match("a]c", "%[a\\]b]%", Some('\\'))?);

        Ok(())
    }

    /// The cache lends out `cache[0]`, so the promote-on-hit is load-bearing
    /// for correctness, not just for speed. These would pass trivially if the
    /// cache were removed, so they exercise it through repeated lookups.
    #[test]
    fn pattern_cache_returns_the_right_entry() -> anyhow::Result<()> {
        // Interleave two patterns so each lookup is a hit on a non-zero index.
        for _ in 0..4 {
            assert!(regex_match("abc", "a", false)?);
            assert!(!regex_match("abc", "z", false)?);
            assert!(regex_match("abc", "b", false)?);
        }
        // Evict past the bound, then come back to the first pattern.
        for i in 0..PATTERN_CACHE_MAX + 8 {
            let p = format!("p{i}");
            assert!(!regex_match("abc", &p, false)?);
        }
        assert!(regex_match("abc", "a", false)?);
        // The same text means different things as a regex and as a SIMILAR TO
        // pattern, so the two must not share a cache entry.
        assert!(regex_match("axc", "a.c", false)?);
        assert!(!similar_to_match("axc", "a.c", Some('\\'))?);
        assert!(similar_to_match("a.c", "a.c", Some('\\'))?);
        // Different escape characters are likewise distinct keys.
        assert!(similar_to_match("a%c", "a$%c", Some('$'))?);
        assert!(!similar_to_match("a%c", "a$%c", Some('\\'))?);

        Ok(())
    }

    /// Every string over `alphabet` of length `max` or shorter. Each word is
    /// built once, by extending the previous length.
    fn words_upto(alphabet: &[char], max: usize) -> Vec<String> {
        let mut level = vec![String::new()];
        let mut out = level.clone();
        for _ in 0..max {
            level = level
                .iter()
                .flat_map(|w| alphabet.iter().map(move |c| format!("{w}{c}")))
                .collect();
            out.extend_from_slice(&level);
        }
        out
    }

    const ESCAPES: [Option<char>; 4] = [None, Some('\\'), Some('%'), Some('_')];

    /// The compiled matcher must reproduce the interpreted one it replaced,
    /// answer for answer — including which patterns are errors. Exhaustive
    /// rather than randomized: no seed, no flakes, and it covers the escape
    /// precedence and empty-run cases a hand-written list would miss.
    #[test]
    fn compiled_like_agrees_with_the_oracle() {
        let patterns = words_upto(&['a', 'b', '%', '_', '\\'], 4);
        let subjects = words_upto(&['a', 'b', '\\'], 4);
        for pattern in &patterns {
            for escape in ESCAPES {
                for ci in [false, true] {
                    for s in &subjects {
                        let got = like(s, pattern, escape, ci);
                        let want = reference_like(s, pattern, escape, ci);
                        match (&got, &want) {
                            (Ok(a), Ok(b)) => {
                                assert_eq!(a, b, "{s:?} LIKE {pattern:?} ESCAPE {escape:?} ci={ci}")
                            }
                            (Err(a), Err(b)) => {
                                assert_eq!(a.sqlstate, b.sqlstate);
                                assert_eq!(a.message, b.message);
                            }
                            _ => panic!("{pattern:?} ESCAPE {escape:?}: {got:?} vs {want:?}"),
                        }
                    }
                }
            }
        }
    }

    /// The compiled matcher must fold exactly as the interpreted one did, which
    /// is what makes the ASCII fast path safe to take. This is a
    /// self-consistency proof, **not** a PostgreSQL one: the oracle shares the
    /// full-Unicode lowering that the `TODO` in [`like`] is about, so answers
    /// here can still be wrong — `'İ' ILIKE '_'` is true in PG and false in
    /// both implementations. Assertions that would pin those divergent answers
    /// as correct belong with the fix, not here.
    #[test]
    fn ilike_folding_agrees_with_the_oracle_on_unicode() -> anyhow::Result<()> {
        let alphabet = ['a', 'A', 'é', 'İ', 'K', 'Σ', 'Ⱥ', 'ⱥ'];
        let patterns = words_upto(&['a', 'A', 'İ', 'K', 'ⱥ', '%', '_'], 3);
        let subjects = words_upto(&alphabet, 3);
        for pattern in &patterns {
            for ci in [false, true] {
                for s in &subjects {
                    assert_eq!(
                        like(s, pattern, Some('\\'), ci)?,
                        reference_like(s, pattern, Some('\\'), ci)?,
                        "{s:?} LIKE {pattern:?} ci={ci}"
                    );
                }
            }
        }
        // `K` (U+212A) lowers to a plain ASCII `k`, so an ASCII-looking pattern
        // matches a non-ASCII subject. This one PG agrees with.
        assert!(like("K", "%k%", Some('\\'), true)?);
        Ok(())
    }

    /// A `Prefix`/`Suffix` subject is classified by the window it compares
    /// rather than in full, so non-ASCII outside that window must not change
    /// the answer — and a subject shorter than the literal must fall back to
    /// the full check rather than be rejected, since lowering can lengthen it.
    #[test]
    fn ilike_windowed_ascii_check_agrees_with_the_oracle() -> anyhow::Result<()> {
        for (s, pattern) in [
            ("abcé", "ABC%"),
            ("éabc", "%ABC"),
            ("Kabc", "%abc"),
            ("abcK", "abc%"),
            ("İ", "i%"),
            ("İ", "%i"),
            ("i", "İ%"),
            ("Ⱥ", "ⱥ%"),
            ("ⱥ", "Ⱥ%"),
            ("é", "ABCDEF%"),
            ("é", "%ABCDEF"),
        ] {
            assert_eq!(
                like(s, pattern, Some('\\'), true)?,
                reference_like(s, pattern, Some('\\'), true)?,
                "{s:?} ILIKE {pattern:?}"
            );
        }
        Ok(())
    }

    /// A pattern that varies per row misses on every lookup. Recording each one
    /// costs more than compiling it, so the cache stops recording — but it must
    /// still let a pattern that later turns hot back in, and it must not change
    /// any answer while it is doing that.
    #[test]
    fn a_thrashing_cache_stops_growing_but_still_recovers() -> anyhow::Result<()> {
        for i in 0..500 {
            let pattern = format!("%p{i}%");
            assert!(like(&format!("xp{i}y"), &pattern, Some('\\'), false)?);
            assert!(!like("zzz", &pattern, Some('\\'), false)?);
        }
        assert!(
            like_cache_len() <= PATTERN_CACHE_MAX,
            "the cache must stay bounded under thrash"
        );
        // A pattern repeated after the thrash has to get back in and stay.
        for _ in 0..PATTERN_CACHE_PROBE * 2 {
            assert!(like("xhoty", "%hot%", Some('\\'), false)?);
        }
        assert!(
            like_cache_misses() == 0,
            "a repeated pattern must register as a hit"
        );
        Ok(())
    }

    /// A pattern that lands in the general path when it should have
    /// specialized still gives the right answer, so nothing else would catch
    /// the compiler silently downgrading.
    #[test]
    fn patterns_compile_to_the_narrowest_shape() -> anyhow::Result<()> {
        let kind = LikeKind {
            escape: Some('\\'),
            case_insensitive: false,
        };
        for (pattern, want) in [
            ("abc", "Exact"),
            ("", "Exact"),
            ("a\\%c", "Exact"),
            ("abc%", "Prefix"),
            ("%abc", "Suffix"),
            ("%google%", "Contains"),
            ("%%google%%", "Contains"),
            ("%", "Contains"),
            ("a_c", "Whole"),
            ("_%", "Segments"),
            ("a%b%c", "Segments"),
        ] {
            assert_eq!(compile_like(pattern, kind)?.shape(), want, "{pattern:?}");
        }
        Ok(())
    }

    /// `_` matches one character, never one byte.
    #[test]
    fn underscore_counts_characters() -> anyhow::Result<()> {
        assert!(like("é", "_", Some('\\'), false)?);
        assert!(!like("é", "__", Some('\\'), false)?);
        assert!(like("🦀", "_", Some('\\'), false)?);
        assert!(!like("🦀", "____", Some('\\'), false)?);
        assert!(like("aéb", "a_b", Some('\\'), false)?);
        assert!(like("xé", "%_", Some('\\'), false)?);
        assert!(like("éx", "_%", Some('\\'), false)?);
        Ok(())
    }

    /// Failed compiles are never cached, so a bad pattern errors on *every*
    /// row rather than only the first. Pinned because caching the `Err` looks
    /// like a free optimization and would silently change which rows fail.
    #[test]
    fn a_bad_pattern_errors_every_time() {
        for _ in 0..3 {
            let Err(err) = like("x", "a\\", Some('\\'), false) else {
                panic!("a pattern ending in the escape character must be rejected");
            };
            assert_eq!(err.sqlstate, sqlstate::INVALID_ESCAPE_SEQUENCE);
            assert_eq!(
                err.message,
                "LIKE pattern must not end with escape character"
            );
        }
    }

    /// The escape and the case flag are part of the key: the same pattern text
    /// compiles to different programs under each, and a missing key field only
    /// shows up when queries interleave.
    #[test]
    fn like_cache_keys_on_escape_and_case() -> anyhow::Result<()> {
        for _ in 0..4 {
            // Under `ESCAPE '%'` the pattern is the literal `ab`; under `\` the
            // `%` is a wildcard.
            assert!(!like("axb", "a%b", Some('%'), false)?);
            assert!(like("axb", "a%b", Some('\\'), false)?);
            assert!(like("ab", "a%b", Some('%'), false)?);
            assert!(!like("ABC", "abc", Some('\\'), false)?);
            assert!(like("ABC", "abc", Some('\\'), true)?);
        }
        // Evict past the bound, then come back to the first pattern.
        for i in 0..PATTERN_CACHE_MAX + 8 {
            assert!(!like("abc", &format!("p{i}"), Some('\\'), false)?);
        }
        assert!(!like("axb", "a%b", Some('%'), false)?);
        Ok(())
    }

    #[test]
    fn flag_completeness() -> anyhow::Result<()> {
        // `t` is the inverse of `x`, and later flags win.
        assert_eq!(regexp_replace("abc", "a b c", "X", "t")?, "abc");
        assert_eq!(regexp_replace("abc", "a b c", "X", "tx")?, "X");
        assert_eq!(regexp_replace("abc", "a b c", "X", "xt")?, "abc");
        // `q` cannot combine with expanded or newline modes, but `s`/`i` are fine.
        for flags in ["qx", "qn", "qm", "qp", "qw"] {
            let e = regexp_like("a b", "a b", flags)
                .expect_err("the literal flag combined with an expanded or newline mode");
            assert_eq!(e.sqlstate, "2201B", "for {flags:?}");
            assert_eq!(
                e.message,
                "invalid regular expression: invalid argument to regex function"
            );
        }
        assert!(regexp_like("a b", "a b", "qs")?);
        assert!(regexp_like("A B", "a b", "qi")?);
        // `b` and `e` select grammars this engine does not speak; both refuse
        // rather than quietly returning the wrong rows.
        for flags in ["b", "e"] {
            assert_eq!(
                regexp_like("abc", "b", flags)
                    .expect_err("a regex grammar flag this engine does not speak")
                    .sqlstate,
                "0A000",
                "for {flags:?}"
            );
        }

        Ok(())
    }

    /// A pattern this engine cannot execute is not the same thing as a pattern
    /// that is malformed: PG accepts both of these.
    #[test]
    fn unsupported_constructs_are_reported_as_such() {
        for pattern in [r"(a)\1", "a(?=b)", "a(?<=b)"] {
            let e = regex_match("aa", pattern, false)
                .expect_err("a backreference or lookaround construct the engine cannot execute");
            assert_eq!(e.sqlstate, "0A000", "for {pattern:?}");
        }
        // A genuinely malformed pattern still reports a syntax error.
        assert_eq!(
            regex_match("aa", "a(", false)
                .expect_err("\"a(\" is malformed, not merely unsupported")
                .sqlstate,
            "2201B"
        );
    }

    /// jsonpath's `like_regex` is XQuery-flavored: `.` does not span a newline
    /// unless `s` is given, the opposite of the `~` operator's default.
    #[test]
    fn jsonpath_like_regex_flags() -> anyhow::Result<()> {
        let f = |s: &str| LikeRegexFlags::parse(s).expect("valid flags");

        assert!(!like_regex_match("a\nb", "a.b", f(""))?);
        assert!(like_regex_match("a\nb", "a.b", f("s"))?);
        assert!(like_regex_match("ABC", "abc", f("i"))?);
        assert!(!like_regex_match("ABC", "abc", f(""))?);
        // `m` anchors at line boundaries without letting `.` span them.
        assert!(like_regex_match("a\nb", "^b", f("m"))?);
        assert!(!like_regex_match("a\nb", "^b", f(""))?);
        assert!(!like_regex_match("a\nb", "a.b", f("m"))?);
        // `q` makes the pattern literal, and composes with `i`.
        assert!(like_regex_match("a.c", "a.c", f("q"))?);
        assert!(!like_regex_match("abc", "a.c", f("q"))?);
        assert!(like_regex_match("A.C", "a.c", f("qi"))?);
        // Non-`s` mode also keeps a negated class from matching a newline.
        assert!(!like_regex_match("\n", "[^x]", f(""))?);
        assert!(like_regex_match("\n", "[^x]", f("s"))?);
        // `~` keeps the POSIX default, so the two disagree by design.
        assert!(regex_match("a\nb", "a.b", false)?);

        Ok(())
    }

    #[test]
    fn like_regex_flags_parse_and_canonicalize() {
        // PG re-emits the parsed set in a fixed order, deduplicated.
        let canon = |s: &str| LikeRegexFlags::parse(s).expect("valid").canonical();
        assert_eq!(canon("qmi"), "imq");
        assert_eq!(canon("xsmiq"), "ismxq");
        assert_eq!(canon("ii"), "i");
        assert_eq!(canon(""), "");
        assert_eq!(canon("xq"), "xq");
        assert!(LikeRegexFlags::parse("").expect("valid").is_empty());
        // The first unrecognized character is reported.
        assert_eq!(LikeRegexFlags::parse("z"), Err('z'));
        assert_eq!(LikeRegexFlags::parse("ihello"), Err('h'));
        assert_eq!(LikeRegexFlags::parse("g"), Err('g'));
    }

    #[test]
    fn like_regex_compiles_at_parse_time() {
        let f = |s: &str| LikeRegexFlags::parse(s).expect("valid flags");

        assert_eq!(
            like_regex_compile("a(", f(""))
                .expect_err("the unclosed group in the like_regex pattern \"a(\"")
                .sqlstate,
            "2201B"
        );
        // Under `q` the pattern is escaped, so it always compiles.
        assert!(like_regex_compile("a(", f("q")).is_ok());
        assert!(like_regex_compile("a.b", f("is")).is_ok());
    }

    #[test]
    fn regexp_parameter_errors() {
        // These functions match at most once, so `g` is rejected — and PG
        // rejects it before it even compiles the pattern.
        for e in [
            regexp_like("abc", "a(", "g").expect_err("the \"g\" flag on regexp_like"),
            regexp_count("abc", "a(", 1, "g").expect_err("the \"g\" flag on regexp_count"),
            regexp_substr("abc", "a(", 1, 1, "g", 0).expect_err("the \"g\" flag on regexp_substr"),
        ] {
            assert_eq!(e.sqlstate, "22023");
            assert!(
                e.message
                    .ends_with("does not support the \"global\" option")
            );
        }
        assert_eq!(
            regexp_count("abc", "b", 0, "")
                .expect_err("a regexp_count \"start\" of 0")
                .message,
            "invalid value for parameter \"start\": 0"
        );
        assert_eq!(
            regexp_substr("abc", "b", 1, 0, "", 0)
                .expect_err("a regexp_substr \"n\" of 0")
                .message,
            "invalid value for parameter \"n\": 0"
        );
        assert_eq!(
            regexp_substr("abc", "b", 1, 1, "", -1)
                .expect_err("a negative regexp_substr \"subexpr\"")
                .message,
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
        // A trailing bare escape has nothing left to escape, and PG drops it
        // rather than complaining.
        assert!(similar_to_match("abc", "abc\\", Some('\\'))?);
        assert!(!similar_to_match("ab", "abc\\", Some('\\'))?);

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
                .expect_err("the unbalanced bracket in \"a[\"")
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
    fn substring_posix_regex() -> anyhow::Result<()> {
        // With no subexpression the whole match is returned.
        assert_eq!(substring_regex("Thomas", "...$")?.as_deref(), Some("mas"));
        assert_eq!(substring_regex("foobar", "o+")?.as_deref(), Some("oo"));
        // With one or more, the *first* is what comes back.
        assert_eq!(substring_regex("foobar", "o(.)b")?.as_deref(), Some("o"));
        assert_eq!(substring_regex("foobar", "o(.)b(a)")?.as_deref(), Some("o"));
        // A group that did not participate is NULL, not the empty string.
        assert_eq!(substring_regex("abc", "(x)?b")?, None);
        // No match at all is NULL, and matching is case-sensitive.
        assert_eq!(substring_regex("abc", "x")?, None);
        assert_eq!(substring_regex("ABC", "b")?, None);

        Ok(())
    }

    #[test]
    fn substring_sql_regex() -> anyhow::Result<()> {
        let esc = Some('#');
        // Two separators delimit the extracted part; the pattern as a whole
        // still has to match the whole string.
        assert_eq!(
            substring_similar("Thomas", "%#\"o_a#\"_", esc)?.as_deref(),
            Some("oma")
        );
        // With no separators the whole match comes back...
        assert_eq!(
            substring_similar("Thomas", "%o_a_", esc)?.as_deref(),
            Some("Thomas")
        );
        // ...and with a lone opening one, everything from there to the end.
        assert_eq!(substring_similar("XY", "X#\"Y", esc)?.as_deref(), Some("Y"));
        // Parentheses written by the user do not shift the separator group.
        assert_eq!(
            substring_similar("abc", "(a)#\"b#\"c", esc)?.as_deref(),
            Some("b")
        );
        // An empty capture is the empty string, distinct from no match at all.
        assert_eq!(
            substring_similar("Thomas", "Thomas#\"#\"", esc)?.as_deref(),
            Some("")
        );
        assert_eq!(substring_similar("Thomas", "o", esc)?, None);
        // Without an escape character `#"` is just two literal characters.
        assert_eq!(substring_similar("Thomas", "%#\"o_a#\"_", None)?, None);
        // A third separator is an error rather than a silent extra group.
        assert_eq!(
            substring_similar("XYZ", "X#\"Y#\"Z#\"", esc)
                .expect_err("a third capture separator in the pattern")
                .sqlstate,
            "2200C"
        );

        // SIMILAR TO shares the translation and simply ignores the separators.
        assert!(similar_to_match("x", "x#\"", esc)?);
        assert!(!similar_to_match("x\"", "x#\"", esc)?);
        assert!(similar_to_match("x", "#\"x#\"", esc)?);

        Ok(())
    }

    /// The separators split the pattern into segments that are wrapped
    /// independently, and the segment before the first one must prefer the
    /// *shortest* match so extraction starts as early as possible. Every
    /// expected value here was taken from PG 18.4.
    #[test]
    fn substring_sql_regex_segments() -> anyhow::Result<()> {
        let esc = Some('#');
        // A greedy prefix would swallow the capture and return "" or a suffix.
        assert_eq!(
            substring_similar("abc", "%#\"%#\"%", esc)?.as_deref(),
            Some("abc")
        );
        assert_eq!(
            substring_similar("aaa", "%#\"a%#\"%", esc)?.as_deref(),
            Some("aaa")
        );
        assert_eq!(
            substring_similar("foobar", "%#\"o+#\"%", esc)?.as_deref(),
            Some("oo")
        );
        assert_eq!(
            substring_similar("abc", "%#\"_#\"%", esc)?.as_deref(),
            Some("a")
        );
        assert_eq!(
            substring_similar("abc", "%#\"%", esc)?.as_deref(),
            Some("abc")
        );
        // The preference has to reach a bound and a user-written quantifier too.
        assert_eq!(
            substring_similar("aaa", "a{1,2}#\"%#\"%", esc)?.as_deref(),
            Some("aa")
        );
        assert_eq!(
            substring_similar("aaab", "a*#\"%#\"%", esc)?.as_deref(),
            Some("aaab")
        );
        // Alternation must not bind across a separator boundary: emitting the
        // separator in place would build `^(?:(a)|b)$`, which cannot match "ab".
        assert_eq!(
            substring_similar("ab", "#\"a#\"|b", esc)?.as_deref(),
            Some("a")
        );
        // The extracted segment prefers the longest match, overriding a lazy
        // marker the user wrote inside it (PG's `{1,1}` wrapper).
        assert_eq!(
            substring_similar("aaa", "#\"a*?#\"%", esc)?.as_deref(),
            Some("aaa")
        );
        // ...and a user marker in the prefix is consumed, never doubled into the
        // `a*???` the regex compiler would choke on.
        assert_eq!(
            substring_similar("ab", "%?#\"a#\"%", esc)?.as_deref(),
            Some("a")
        );
        assert_eq!(
            substring_similar("x\u{65e5}y", "%#\"\u{65e5}#\"%", esc)?.as_deref(),
            Some("\u{65e5}")
        );

        // A quantifier straight after the closing separator has no operand, so
        // the suffix `(?:{3})` is rejected like PG's `quantifier operand invalid`.
        assert_eq!(
            substring_similar("aaa", "#\"a#\"{3}", esc)
                .expect_err("a bound with no operand after the closing separator")
                .sqlstate,
            "2201B"
        );
        assert_eq!(
            similar_to_match("aaa", "#\"a#\"{3}", esc)
                .expect_err("the operandless {3} in a SIMILAR TO pattern")
                .sqlstate,
            "2201B"
        );
        // Digits after `{` commit PG to reading a bound, so leaving it unclosed
        // is an error rather than a literal brace.
        assert_eq!(
            similar_to_match("a{1", "a{1", esc)
                .expect_err("the unclosed bound in \"a{1\"")
                .sqlstate,
            "2201B"
        );

        // A separator inside a bracket expression is an ordinary class member.
        assert!(similar_to_match("a\"b", "a[#\"]b", esc)?);
        // Empty segments in every position.
        assert_eq!(substring_similar("X", "X#\"#\"", esc)?.as_deref(), Some(""));
        assert_eq!(substring_similar("", "#\"#\"", esc)?.as_deref(), Some(""));
        assert_eq!(
            substring_similar("ab", "#\"%#\"", esc)?.as_deref(),
            Some("ab")
        );

        Ok(())
    }

    /// The escape character does not make the next character a literal: PG
    /// re-emits it as an ARE escape, so `#d` is the digit class rather than the
    /// letter `d`. Values checked against PG 18.4.
    #[test]
    fn similar_to_are_escapes() -> anyhow::Result<()> {
        let esc = Some('#');
        // Class shorthands.
        assert!(similar_to_match("5", "#d", esc)?);
        assert!(!similar_to_match("d", "#d", esc)?);
        assert!(similar_to_match("d", "#D", esc)?);
        assert!(similar_to_match("\t", "#s", esc)?);
        assert!(similar_to_match("a", "#w", esc)?);
        // Character escapes, including the two the regex crate spells
        // differently: PG's `\b` is a backspace and its `\B` a backslash.
        assert!(similar_to_match("\u{7}", "#a", esc)?);
        assert!(similar_to_match("\u{8}", "#b", esc)?);
        assert!(similar_to_match("\\", "#B", esc)?);
        assert!(similar_to_match("\u{1b}", "#e", esc)?);
        assert!(similar_to_match("\t", "#t", esc)?);
        assert!(similar_to_match("\n", "#n", esc)?);
        // Numeric escapes: hex is variable width, `\u`/`\U` are fixed, and `\0`
        // opens an octal run.
        assert!(similar_to_match("A", "#x41", esc)?);
        assert!(similar_to_match("\u{7}", "#x7", esc)?);
        assert!(similar_to_match("A", "#u0041", esc)?);
        assert!(similar_to_match("A", "#U00000041", esc)?);
        assert!(similar_to_match("\u{1}", "#cA", esc)?);
        assert!(similar_to_match("\u{1}", "#01", esc)?);
        // Word-boundary constraints.
        assert!(similar_to_match("a b", "a#y #yb", esc)?);
        assert!(!similar_to_match("ab", "#ya#yb", esc)?);
        // Punctuation after the escape is still just that character.
        assert!(similar_to_match("%", "#%", esc)?);
        assert!(similar_to_match("#", "##", esc)?);

        // An undefined letter escape is an error, as in PG, and a backreference
        // is reported as the unsupported construct it is.
        assert_eq!(
            similar_to_match("x", "#q", esc)
                .expect_err("the undefined letter escape \"#q\"")
                .sqlstate,
            "2201B"
        );
        assert_eq!(
            similar_to_match("x", "#u41", esc)
                .expect_err("the too-short unicode escape \"#u41\"")
                .sqlstate,
            "2201B"
        );
        assert_eq!(
            similar_to_match("aa", "(a)#1", esc)
                .expect_err("the backreference \"#1\"")
                .sqlstate,
            "0A000"
        );

        // The escape keeps working inside a bracket expression: `[a#"b]` is the
        // class {a, ", b} and does not contain the escape character itself.
        assert!(similar_to_match("\"", "[a#\"b]", esc)?);
        assert!(!similar_to_match("#", "[a#\"b]", esc)?);
        assert!(similar_to_match("5", "[#d]", esc)?);
        assert!(similar_to_match("\u{7}", "[#a]", esc)?);
        // An escaped `]` is a class member, not the end of the class.
        assert!(similar_to_match("a]b", "[a#]b]%", esc)?);
        // Zero-width constraints are not members, and PG rejects them there.
        assert_eq!(
            similar_to_match("a", "[#y]", esc)
                .expect_err("the word-boundary constraint inside the class \"[#y]\"")
                .sqlstate,
            "2201B"
        );

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
            decode("abc", "base64")
                .expect_err("the unpadded base64 input \"abc\"")
                .message,
            "invalid base64 end sequence"
        );
        assert_eq!(
            decode("a@b", "base64")
                .expect_err("the \"@\" symbol in base64 input")
                .message,
            "invalid symbol \"@\" found while decoding base64 sequence"
        );
        assert_eq!(
            decode("xy", "hex")
                .expect_err("the non-hex digits in \"xy\"")
                .message,
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
            format("%", &[])
                .expect_err("a trailing \"%\" with no type specifier")
                .message,
            "unterminated format() type specifier"
        );
        assert_eq!(
            format("%0$s", &[Some("x".into())])
                .expect_err("argument position 0 in a format specifier")
                .message,
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
            varchar_input("abcdef", 3, false)
                .expect_err("a value too long for varchar(3) without an explicit cast")
                .sqlstate,
            "22001"
        );
        assert_eq!(bpchar_rtrim("ab   "), "ab");

        Ok(())
    }
}
