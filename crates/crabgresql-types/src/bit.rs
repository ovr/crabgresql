//! `bit(n)` / `bit varying(n)` (varbit): input parsing, operators, and the
//! functions the `bit` regression suite exercises.
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the `0`/`1` output, the accepted input spellings, the length
//! rules, and the SQLSTATE/message of each error — implemented independently.
//!
//! Representation: a bit string of `len` bits is stored as `ceil(len/8)` bytes,
//! most-significant bit first (bit 0 is the high bit of byte 0), with the unused
//! trailing bits of the last byte held at zero. This mirrors PG's `VarBit`
//! layout and keeps arbitrary widths (the suite uses `bit(1000)`). Operators are
//! implemented over an unpacked `Vec<bool>` for clarity and repacked on the way
//! out — cheap at these sizes.

use std::cmp::Ordering;

// SQLSTATEs (kept as literals; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_TEXT_REPRESENTATION: &str = "22P02";
const STRING_DATA_LENGTH_MISMATCH: &str = "22026";
const SUBSTRING_ERROR: &str = "22011";
const ARRAY_SUBSCRIPT_ERROR: &str = "2202E";

/// A bit-string error, carrying the SQLSTATE and message PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct BitError {
    pub sqlstate: &'static str,
    pub message: String,
}

impl BitError {
    fn new(sqlstate: &'static str, message: impl Into<String>) -> BitError {
        BitError { sqlstate, message: message.into() }
    }
}

// --- packing helpers -------------------------------------------------------

/// Unpack `len` bits from `data` (MSB-first) into a boolean vector.
fn to_bits(len: u32, data: &[u8]) -> Vec<bool> {
    (0..len as usize)
        .map(|i| (data[i / 8] >> (7 - (i % 8))) & 1 == 1)
        .collect()
}

/// Pack a boolean bit vector back into `(len, data)` (MSB-first, trailing pad
/// bits zero).
fn from_bits(bits: &[bool]) -> (u32, Vec<u8>) {
    let mut data = vec![0u8; bits.len().div_ceil(8)];
    for (i, &b) in bits.iter().enumerate() {
        if b {
            data[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    (bits.len() as u32, data)
}

// --- input functions -------------------------------------------------------

/// Parse a run of `0`/`1` characters (the `B'...'` literal body, or the default
/// text input format). Any other character is `22P02`, naming the offender.
pub fn from_binary(s: &str) -> Result<(u32, Vec<u8>), BitError> {
    let mut bits = Vec::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '0' => bits.push(false),
            '1' => bits.push(true),
            other => {
                return Err(BitError::new(
                    INVALID_TEXT_REPRESENTATION,
                    format!("\"{other}\" is not a valid binary digit"),
                ));
            }
        }
    }
    Ok(from_bits(&bits))
}

/// Parse a run of hex digits (the `X'...'` literal body, or the `x`-prefixed
/// text input format): each hex digit contributes four bits, MSB-first. Any
/// non-hex character is `22P02`, naming the offender.
pub fn from_hex(s: &str) -> Result<(u32, Vec<u8>), BitError> {
    let mut bits = Vec::with_capacity(s.len() * 4);
    for c in s.chars() {
        let nibble = c.to_digit(16).ok_or_else(|| {
            BitError::new(
                INVALID_TEXT_REPRESENTATION,
                format!("\"{c}\" is not a valid hexadecimal digit"),
            )
        })?;
        for shift in (0..4).rev() {
            bits.push((nibble >> shift) & 1 == 1);
        }
    }
    Ok(from_bits(&bits))
}

/// `bit_in` / `varbit_in`: the text input function. A leading `x`/`X` selects
/// hex; a leading `b`/`B` selects binary; otherwise the body is binary. Shared
/// by the binder's `parse_unknown` and the executor's `text -> bit` cast.
pub fn input(s: &str) -> Result<(u32, Vec<u8>), BitError> {
    match s.as_bytes().first() {
        Some(b'x' | b'X') => from_hex(&s[1..]),
        Some(b'b' | b'B') => from_binary(&s[1..]),
        _ => from_binary(s),
    }
}

/// `bit_out` / `varbit_out`: the `0`/`1` text form, MSB-first. A zero-length
/// string prints empty.
pub fn format(len: u32, data: &[u8]) -> String {
    to_bits(len, data).into_iter().map(|b| if b { '1' } else { '0' }).collect()
}

// --- length coercion -------------------------------------------------------

/// Apply a `bit(n)` (`varying = false`) or `bit varying(n)` (`varying = true`)
/// typmod. `typmod < 0` means no limit. In assignment context (`explicit =
/// false`) a length mismatch (fixed) or overflow (varying) errors; an explicit
/// cast truncates, and a fixed cast zero-pads a too-short value on the right.
pub fn coerce(
    len: u32,
    data: &[u8],
    typmod: i32,
    varying: bool,
    explicit: bool,
) -> Result<(u32, Vec<u8>), BitError> {
    if typmod < 0 {
        return Ok((len, data.to_vec()));
    }
    let n = typmod as u32;
    if varying {
        if len <= n {
            return Ok((len, data.to_vec()));
        }
        if !explicit {
            return Err(BitError::new(
                STRING_DATA_LENGTH_MISMATCH,
                format!("bit string too long for type bit varying({n})"),
            ));
        }
    } else {
        if len == n {
            return Ok((len, data.to_vec()));
        }
        if !explicit {
            return Err(BitError::new(
                STRING_DATA_LENGTH_MISMATCH,
                format!("bit string length {len} does not match type bit({n})"),
            ));
        }
    }
    // Explicit cast (or fixed pad/truncate): resize to exactly `n` bits, keeping
    // the leading bits and zero-filling on the right.
    let mut bits = to_bits(len, data);
    bits.resize(n as usize, false);
    Ok(from_bits(&bits))
}

// --- comparison ------------------------------------------------------------

/// `bit_cmp` / `varbit_cmp`: compare the common-length prefix bit-by-bit; if all
/// equal, the shorter string sorts first.
pub fn cmp(la: u32, da: &[u8], lb: u32, db: &[u8]) -> Ordering {
    let a = to_bits(la, da);
    let b = to_bits(lb, db);
    a.cmp(&b)
}

// --- bitwise operators -----------------------------------------------------

/// `~` (bitwise NOT), keeping the length.
pub fn not(len: u32, data: &[u8]) -> (u32, Vec<u8>) {
    let bits: Vec<bool> = to_bits(len, data).into_iter().map(|b| !b).collect();
    from_bits(&bits)
}

fn binary_op(
    verb: &str,
    la: u32,
    da: &[u8],
    lb: u32,
    db: &[u8],
    f: impl Fn(bool, bool) -> bool,
) -> Result<(u32, Vec<u8>), BitError> {
    if la != lb {
        return Err(BitError::new(
            STRING_DATA_LENGTH_MISMATCH,
            format!("cannot {verb} bit strings of different sizes"),
        ));
    }
    let a = to_bits(la, da);
    let b = to_bits(lb, db);
    let bits: Vec<bool> = a.iter().zip(&b).map(|(&x, &y)| f(x, y)).collect();
    Ok(from_bits(&bits))
}

/// `&` (bitwise AND); errors on differing sizes.
pub fn and(la: u32, da: &[u8], lb: u32, db: &[u8]) -> Result<(u32, Vec<u8>), BitError> {
    binary_op("AND", la, da, lb, db, |x, y| x & y)
}

/// `|` (bitwise OR); errors on differing sizes.
pub fn or(la: u32, da: &[u8], lb: u32, db: &[u8]) -> Result<(u32, Vec<u8>), BitError> {
    binary_op("OR", la, da, lb, db, |x, y| x | y)
}

/// `#` (bitwise XOR); errors on differing sizes.
pub fn xor(la: u32, da: &[u8], lb: u32, db: &[u8]) -> Result<(u32, Vec<u8>), BitError> {
    binary_op("XOR", la, da, lb, db, |x, y| x ^ y)
}

/// `||` (concatenation): the bits of `a` followed by the bits of `b`.
pub fn concat(la: u32, da: &[u8], lb: u32, db: &[u8]) -> (u32, Vec<u8>) {
    let mut bits = to_bits(la, da);
    bits.extend(to_bits(lb, db));
    from_bits(&bits)
}

/// `<<` (shift left, toward the MSB): the result keeps the input length, with
/// zeros shifted in on the right. A negative shift is a shift right.
pub fn shift_left(len: u32, data: &[u8], shift: i32) -> (u32, Vec<u8>) {
    let src = to_bits(len, data);
    let n = len as i64;
    let s = shift as i64;
    let bits: Vec<bool> = (0..n)
        .map(|i| {
            let j = i + s;
            (0..n).contains(&j) && src[j as usize]
        })
        .collect();
    from_bits(&bits)
}

/// `>>` (shift right): keeps the length, zeros in on the left. Negative is left.
pub fn shift_right(len: u32, data: &[u8], shift: i32) -> (u32, Vec<u8>) {
    shift_left(len, data, shift.wrapping_neg())
}

// --- functions -------------------------------------------------------------

/// `length(bit)` — the number of bits.
pub fn length(len: u32) -> i32 {
    len as i32
}

/// `bit_count(bit)` — the number of set bits (int8).
pub fn bit_count(len: u32, data: &[u8]) -> i64 {
    to_bits(len, data).iter().filter(|&&b| b).count() as i64
}

/// `get_bit(bit, n)` — the bit at 0-based index `n`; out of range is `2202E`.
pub fn get_bit(len: u32, data: &[u8], n: i32) -> Result<i32, BitError> {
    if n < 0 || n as u32 >= len {
        return Err(index_out_of_range(n, len));
    }
    let n = n as usize;
    Ok(((data[n / 8] >> (7 - (n % 8))) & 1) as i32)
}

/// `set_bit(bit, n, newvalue)` — set the bit at 0-based index `n`.
pub fn set_bit(len: u32, data: &[u8], n: i32, value: i32) -> Result<(u32, Vec<u8>), BitError> {
    if n < 0 || n as u32 >= len {
        return Err(index_out_of_range(n, len));
    }
    let mut bits = to_bits(len, data);
    bits[n as usize] = value != 0;
    Ok(from_bits(&bits))
}

fn index_out_of_range(n: i32, len: u32) -> BitError {
    BitError::new(
        ARRAY_SUBSCRIPT_ERROR,
        format!("bit index {n} out of valid range (0..{})", len as i32 - 1),
    )
}

/// `POSITION(sub IN str)` for bit strings: the 1-based index of the first
/// occurrence of `sub` within `str`, or 0 if absent. An empty `str` yields 0; a
/// (non-empty `str`, empty `sub`) yields 1.
pub fn position(str_len: u32, str_data: &[u8], sub_len: u32, sub_data: &[u8]) -> i32 {
    if str_len == 0 {
        return 0;
    }
    if sub_len == 0 {
        return 1;
    }
    if sub_len > str_len {
        return 0;
    }
    let haystack = to_bits(str_len, str_data);
    let needle = to_bits(sub_len, sub_data);
    let last = (str_len - sub_len) as usize;
    for start in 0..=last {
        if haystack[start..start + needle.len()] == needle[..] {
            return start as i32 + 1;
        }
    }
    0
}

/// `substring(bit FROM s [FOR l])` — `bitsubstr`. `s` is 1-based; a negative
/// `for` length errors (`22011`). Arithmetic uses i64 to tolerate the i32
/// overflow the suite probes.
pub fn substring(
    len: u32,
    data: &[u8],
    start: i32,
    length: Option<i32>,
) -> Result<(u32, Vec<u8>), BitError> {
    let bitlen = len as i64;
    let s = start as i64;
    let s1 = s.max(1);
    let e1 = match length {
        None => bitlen + 1,
        Some(l) => {
            let e = s + l as i64;
            if e < s {
                return Err(BitError::new(
                    SUBSTRING_ERROR,
                    "negative substring length not allowed",
                ));
            }
            e.min(bitlen + 1)
        }
    };
    if s1 > bitlen || e1 < 1 || e1 <= s1 {
        return Ok(from_bits(&[]));
    }
    let bits = to_bits(len, data);
    let from = (s1 - 1) as usize;
    let to = (e1 - 1) as usize;
    Ok(from_bits(&bits[from..to]))
}

/// `overlay(bit placing repl from sp [for sl])` — replace `sl` bits of `str`
/// starting at 1-based `sp` with `repl` (`sl` defaults to `repl`'s length).
pub fn overlay(
    str_len: u32,
    str_data: &[u8],
    repl_len: u32,
    repl_data: &[u8],
    sp: i32,
    sl: Option<i32>,
) -> Result<(u32, Vec<u8>), BitError> {
    let sl = sl.unwrap_or(repl_len as i32);
    // head = str[1 .. sp-1], tail = str[sp+sl .. end]
    let (hl, hd) = substring(str_len, str_data, 1, Some(sp - 1))?;
    let (tl, td) = substring(str_len, str_data, sp + sl, None)?;
    let (ml, md) = concat(hl, &hd, repl_len, repl_data);
    Ok(concat(ml, &md, tl, &td))
}

/// Reinterpret the `len` leading bits as an unsigned big-endian integer (bit 0
/// is the most significant), for the `bit -> int4`/`int8` casts.
pub fn to_u64(len: u32, data: &[u8]) -> u64 {
    let mut acc = 0u64;
    for b in to_bits(len, data) {
        acc = (acc << 1) | b as u64;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(len: u32, data: &[u8]) -> String {
        format(len, data)
    }

    #[test]
    fn parse_and_format_roundtrip() {
        let (l, d) = from_binary("11011000000").unwrap();
        assert_eq!(l, 11);
        assert_eq!(s(l, &d), "11011000000");
        let (l, d) = from_binary("").unwrap();
        assert_eq!(s(l, &d), "");
        let (l, d) = from_hex("0F").unwrap();
        assert_eq!(s(l, &d), "00001111");
        let (l, d) = from_hex("2468").unwrap();
        assert_eq!(s(l, &d), "0010010001101000");
    }

    #[test]
    fn parse_rejects_bad_digits() {
        assert_eq!(from_binary(" 0").unwrap_err().message, "\" \" is not a valid binary digit");
        assert_eq!(from_hex("Z").unwrap_err().message, "\"Z\" is not a valid hexadecimal digit");
        assert_eq!(input("01010Z01").unwrap_err().message, "\"Z\" is not a valid binary digit");
        assert_eq!(
            input("x01010Z01").unwrap_err().message,
            "\"Z\" is not a valid hexadecimal digit"
        );
    }

    #[test]
    fn coerce_fixed_and_varying() {
        let (l, d) = from_binary("10").unwrap();
        // fixed, assignment, wrong length
        assert_eq!(
            coerce(l, &d, 11, false, false).unwrap_err().message,
            "bit string length 2 does not match type bit(11)"
        );
        // varying, assignment, too long
        let (l2, d2) = from_binary("101011111010").unwrap();
        assert_eq!(
            coerce(l2, &d2, 11, true, false).unwrap_err().message,
            "bit string too long for type bit varying(11)"
        );
        // explicit fixed truncation
        let (l3, d3) = coerce(l2, &d2, 8, false, true).unwrap();
        assert_eq!(s(l3, &d3), "10101111");
    }

    #[test]
    fn ops_match_pg() {
        let (la, da) = from_hex("0F").unwrap(); // 00001111
        let (lb, db) = from_hex("10").unwrap(); // 00010000
        assert_eq!({ let (l, d) = not(la, &da); s(l, &d) }, "11110000");
        assert_eq!({ let (l, d) = and(la, &da, lb, &db).unwrap(); s(l, &d) }, "00000000");
        assert_eq!({ let (l, d) = or(la, &da, lb, &db).unwrap(); s(l, &d) }, "00011111");
        assert_eq!({ let (l, d) = xor(la, &da, lb, &db).unwrap(); s(l, &d) }, "00011111");
        assert_eq!({ let (l, d) = shift_left(la, &da, 4); s(l, &d) }, "11110000");
        assert_eq!({ let (l, d) = shift_right(lb, &db, 2); s(l, &d) }, "00000100");
        assert_eq!(
            and(la, &da, 3, &from_binary("101").unwrap().1).unwrap_err().message,
            "cannot AND bit strings of different sizes"
        );
    }

    #[test]
    fn substring_and_overflow() {
        let (l, d) = from_binary("01010101").unwrap();
        assert_eq!({ let (l, d) = substring(l, &d, 2, Some(2147483646)).unwrap(); s(l, &d) }, "1010101");
        assert_eq!({ let (l, d) = substring(l, &d, -10, Some(2147483646)).unwrap(); s(l, &d) }, "01010101");
        assert_eq!(
            substring(l, &d, -10, Some(-2147483646)).unwrap_err().message,
            "negative substring length not allowed"
        );
    }

    #[test]
    fn position_boundaries() {
        let p = |sub: &str, str_: &str| {
            let (sl, sd) = from_binary(sub).unwrap();
            let (tl, td) = from_binary(str_).unwrap();
            position(tl, &td, sl, &sd)
        };
        assert_eq!(p("1010", "0000101"), 0);
        assert_eq!(p("1010", "00001010"), 5);
        assert_eq!(p("", "00001010"), 1);
        assert_eq!(p("0", ""), 0);
        assert_eq!(p("", ""), 0);
    }

    #[test]
    fn get_set_bit() {
        let (l, d) = from_binary("0101011000100").unwrap();
        assert_eq!(get_bit(l, &d, 10).unwrap(), 1);
        let (l, d) = from_binary("0101011000100100").unwrap();
        let (l2, d2) = set_bit(l, &d, 15, 1).unwrap();
        assert_eq!(s(l2, &d2), "0101011000100101");
        assert_eq!(
            set_bit(l, &d, 16, 1).unwrap_err().message,
            "bit index 16 out of valid range (0..15)"
        );
    }

    #[test]
    fn overlay_and_count() {
        let (l, d) = from_binary("0101011100").unwrap();
        let (rl, rd) = from_binary("001").unwrap();
        let (ol, od) = overlay(l, &d, rl, &rd, 2, Some(3)).unwrap();
        assert_eq!(s(ol, &od), "0001011100");
        let (ol, od) = overlay(l, &d, from_binary("101").unwrap().0, &from_binary("101").unwrap().1, 6, None).unwrap();
        assert_eq!(s(ol, &od), "0101010100");
        assert_eq!(bit_count(l, &d), 5);
    }
}
