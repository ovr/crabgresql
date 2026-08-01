//! `oidvector` / `int2vector`: the fixed-element-type vectors PostgreSQL uses in
//! its own catalogs (`pg_proc.proargtypes`, `pg_index.indkey`, ...).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the accepted input spellings, the space-separated output, and the
//! SQLSTATE/message of each rejection. Every rule below was derived by probing
//! PostgreSQL 18.4 directly, not read off its source; the cases are pinned by
//! the tests at the bottom of this file.
//!
//! Representation: `Value::Vector { kind, elems }`, where `elems` are
//! `Value::Oid` or `Value::Int2` and never NULL.
//!
//! The two types look alike but share almost no rules, and every difference
//! below is observable:
//!
//! | | `oidvector` | `int2vector` |
//! |---|---|---|
//! | element scan | C's `strtoul(s, &end, 0)`, so `'0x1f 010'` is `31 8` | decimal only, so `'0x10'` is an error |
//! | element boundary | wherever the previous scan stopped, so `'08'` is **two** elements `0 8` and `'1-2'` is `1 4294967294` | whitespace-delimited, so `'08'` is the one element `8` |
//! | separators | C's `isspace`, including tab and vertical tab | **space only** — `E'1\t2'` is an error |
//! | ordering | element **count** first (`btoidvectorcmp`), so `'2' < '1 1'` | element-wise (no opclass of its own; falls back to the polymorphic array ordering), so `'2' > '1 1'` |
//!
//! What they do share: an empty input yields an empty vector (which prints as
//! ``), there is no cap on the element count, and a rejection quotes the input
//! from the offending position through to the end of the *whole* string while
//! naming the **element** type — `oid` or `smallint`, never `oidvector`.
//!
//! # Divergences
//!
//! * `oidvector::oid[]` is unsupported. PostgreSQL yields a **0-based** array,
//!   printing `'1 2'::oidvector::oid[]` as `[0:1]={1,2}`. [`crate::Value::Array`]
//!   has no lower-bound concept, so rendering it as `{1,2}` would be a silently
//!   different value; the cast raises `42846` instead.
//! * `oid[]::oidvector` and `array[]::oidvector` are errors in PostgreSQL too
//!   (`cannot cast type oid[] to oidvector`, `array is not a valid oidvector`),
//!   and stay errors here.
//! * PostgreSQL's polymorphic `anyarray` functions and operators accept these
//!   types, because `typelem` is set and `typlen` is -1 — so `cardinality`,
//!   `array_length`, `@>`, `<@` and `&&` all work on an `oidvector` there.
//!   Here they are gated on [`crate::PgType::Array`] and raise `42883`.
//!   `unnest` and subscripting are wired up individually and do work.
//!
//! Note that *subscripting* is 0-based, unlike a real array:
//! `('11 22 33'::oidvector)[0]` is `11`. That is handled in the executor's
//! `Subscript` evaluation, not here.

use crate::cast::{CastError, invalid_input, value_out_of_range};
use crate::xid::{ScanError, scan_prefix};
use crate::{PgType, Value};

/// Which element type a vector holds. Each variant is a distinct PostgreSQL
/// type; they share one [`crate::PgType`] variant because they differ only in
/// the element type and its input function, the same way [`crate::RegKind`]
/// factors the `reg*` family.
#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorKind {
    /// `oidvector`: elements are `oid`.
    Oid,
    /// `int2vector`: elements are `smallint`.
    Int2,
}

impl VectorKind {
    /// The vector type's own OID.
    pub fn oid(self) -> u32 {
        match self {
            VectorKind::Oid => crate::oid::OIDVECTOR,
            VectorKind::Int2 => crate::oid::INT2VECTOR,
        }
    }

    /// `pg_type.typelem`: the element type, which is also what subscripting and
    /// `unnest` yield.
    pub fn element(self) -> PgType {
        match self {
            VectorKind::Oid => PgType::Oid,
            VectorKind::Int2 => PgType::Int2,
        }
    }

    /// Catalog `typname`, which for these is also the SQL spelling.
    pub fn typname(self) -> &'static str {
        match self {
            VectorKind::Oid => "oidvector",
            VectorKind::Int2 => "int2vector",
        }
    }
}

/// `oidvectorout` / `int2vectorout`: the elements, space-separated, with no
/// braces and no delimiter escaping. An empty vector prints as the empty string.
pub fn format(elems: &[Value]) -> String {
    let mut out = String::new();
    for (i, elem) in elems.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match elem {
            Value::Oid(v) => out.push_str(&v.to_string()),
            Value::Int2(v) => out.push_str(&v.to_string()),
            // Vectors are built only by `vector_in` and the catalog, both of
            // which produce element values of the kind's own type.
            other => unreachable!("vector element is not oid/int2: {other:?}"),
        }
    }
    out
}

/// `oidvectorin` / `int2vectorin`. An empty (or all-separator) input yields an
/// empty vector; every rejection quotes the input from the offending position
/// through to the end of the *whole* string, and names the element type.
pub fn vector_in(input: &str, kind: VectorKind) -> Result<Vec<Value>, CastError> {
    match kind {
        VectorKind::Oid => oidvector_in(input),
        VectorKind::Int2 => int2vector_in(input),
    }
}

/// C's `isspace` over ASCII. Rust's `is_ascii_whitespace` is *not* the same
/// set — it omits vertical tab (0x0B), which `oidvectorin` does treat as a
/// separator: `E'11\x0b22'::oidvector` is `11 22`.
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// `oidvectorin`: repeatedly skip separators and scan one `strtoul(s, &end, 0)`
/// element, resuming wherever the previous scan stopped.
///
/// There is no notion of a whitespace-delimited "token" here, and that is
/// observable: `'08'` is *two* elements, `0` then `8`, because the octal scan
/// of `0` stops at the invalid digit `8` and the next scan starts there.
/// Likewise `'1-2'` is `1 4294967294` and `'12+3'` is `12 3`. An element is
/// rejected only when a scan converts nothing at all.
///
/// Each element is then range-checked by `oidin`'s band — which is the same one
/// `xid` uses: any value that fits `u32`, plus the negatives that wrap into it.
/// So `'-1'` is `4294967295` while `'4294967296'` and `'-2147483649'` fall in
/// the gap between the two bands and are rejected.
fn oidvector_in(input: &str) -> Result<Vec<Value>, CastError> {
    let bytes = input.as_bytes();
    let mut elems = Vec::new();
    let mut i = 0;
    loop {
        while i < bytes.len() && is_c_space(bytes[i]) {
            i += 1;
        }
        if i == bytes.len() {
            return Ok(elems);
        }
        let (scanned, consumed) = scan_prefix(&input[i..]);
        match scanned {
            Ok(v) if v <= u64::from(u32::MAX) || v >= (i32::MIN as i64) as u64 => {
                elems.push(Value::Oid(v as u32));
                // `consumed` is at least 1 on the `Ok` path, so this always
                // advances and the loop cannot spin.
                i += consumed;
            }
            Ok(_) | Err(ScanError::Range) => {
                return Err(value_out_of_range(PgType::Oid, &input[i..]));
            }
            Err(ScanError::Syntax) => return Err(invalid_input(PgType::Oid, &input[i..])),
        }
    }
}

/// `int2vectorin`: split on **spaces only** — not tabs or newlines, unlike
/// `oidvectorin` (`E'1\t2'::int2vector` is an error, `E'1\t2'::oidvector` is
/// `1 2`) — and require each whole element to be a decimal `int2`.
///
/// There is no `strtoul` here either, so no hex or octal: `'0x10'` is an error
/// and `'08'` is the single element `8`, not `0 8`.
fn int2vector_in(input: &str) -> Result<Vec<Value>, CastError> {
    let bytes = input.as_bytes();
    let mut elems = Vec::new();
    let mut i = 0;
    loop {
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        if i == bytes.len() {
            return Ok(elems);
        }
        let rest = &input[i..];
        let end = rest.find(' ').unwrap_or(rest.len());
        let token = &rest[..end];
        let digits = token.strip_prefix(['+', '-']).unwrap_or(token);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid_input(PgType::Int2, rest));
        }
        match token.parse::<i16>() {
            Ok(v) => elems.push(Value::Int2(v)),
            Err(_) => return Err(value_out_of_range(PgType::Int2, rest)),
        }
        i += end;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn oids(input: &str) -> Result<String, CastError> {
        Ok(format(&vector_in(input, VectorKind::Oid)?))
    }

    fn int2s(input: &str) -> Result<String, CastError> {
        Ok(format(&vector_in(input, VectorKind::Int2)?))
    }

    #[test]
    fn splits_on_separator_runs_and_trims() -> anyhow::Result<()> {
        assert_eq!(oids(" 1 2  4 ")?, "1 2 4");
        assert_eq!(oids("")?, "");
        assert_eq!(oids("   ")?, "");
        assert_eq!(int2s(" 1  2   3 ")?, "1 2 3");
        assert_eq!(int2s("")?, "");
        // No cap on the element count: PG accepts well past FUNC_MAX_ARGS.
        let many = (1..=101).map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(oids(&many.join(" "))?, many.join(" "));

        Ok(())
    }

    /// `oidvector` separates on C's `isspace` — which includes vertical tab,
    /// the one character Rust's `is_ascii_whitespace` omits. `int2vector`
    /// separates on the space character alone.
    #[test]
    fn the_two_kinds_have_different_separator_sets() -> anyhow::Result<()> {
        for sep in [" ", "\t", "\n", "\x0b", "\x0c", "\r"] {
            assert_eq!(oids(&format!("7{sep}8"))?, "7 8", "oidvector sep {sep:?}");
        }
        for sep in ["\t", "\n", "\x0b", "\x0c", "\r"] {
            let e = vector_in(&format!("7{sep}8"), VectorKind::Int2).unwrap_err();
            assert_eq!(e.sqlstate, "22P02", "int2vector sep {sep:?}");
        }

        Ok(())
    }

    /// `oidvectorin` has no whitespace-delimited "token": each scan resumes
    /// where the last one stopped, so a trailing character that itself converts
    /// simply starts the next element.
    #[test]
    fn oidvector_resumes_scanning_where_the_last_element_stopped() -> anyhow::Result<()> {
        // The octal scan of `0` stops at the invalid digit `8`, which then
        // converts on its own.
        assert_eq!(oids("08")?, "0 8");
        assert_eq!(oids("1 08 9")?, "1 0 8 9");
        assert_eq!(oids("1-2")?, "1 4294967294");
        assert_eq!(oids("12+3")?, "12 3");
        // int2vector is token-based, so the same input is one element.
        assert_eq!(int2s("08")?, "8");
        assert!(vector_in("1-2", VectorKind::Int2).is_err());

        Ok(())
    }

    /// `oidvector` inherits `oidin`'s `strtoul(s, &end, 0)` scan, so hex and
    /// octal spellings convert and a negative wraps into the unsigned range.
    #[test]
    fn oidvector_elements_scan_like_strtoul_base_zero() -> anyhow::Result<()> {
        assert_eq!(oids("1 0x1f 010")?, "1 31 8");
        assert_eq!(oids("0X1F")?, "31");
        assert_eq!(oids("-1")?, "4294967295");
        assert_eq!(oids("+42 -0")?, "42 0");
        assert_eq!(oids("-2147483648")?, "2147483648");
        assert_eq!(oids("18446744073709551615")?, "4294967295");

        Ok(())
    }

    /// `int2vector` is decimal-only — the one place the two kinds accept
    /// genuinely different text.
    #[test]
    fn int2vector_elements_are_decimal_only() -> anyhow::Result<()> {
        assert_eq!(int2s("1 010")?, "1 10");
        assert_eq!(int2s("+5 -0")?, "5 0");
        assert_eq!(int2s("-32768 32767")?, "-32768 32767");
        let e = vector_in("0x10 -1", VectorKind::Int2).unwrap_err();
        assert_eq!(e.sqlstate, "22P02");
        assert_eq!(
            e.message,
            "invalid input syntax for type smallint: \"0x10 -1\""
        );

        Ok(())
    }

    /// PG names the *element* type, and quotes the input from the scan stop
    /// through to the end of the whole string.
    #[test]
    fn oidvector_syntax_errors_quote_from_the_scan_stop() {
        for (input, quoted) in [
            ("01 01XYZ", "XYZ"),
            ("1 34junk 9", "junk 9"),
            ("1 ,2 3", ",2 3"),
            ("1 +5x", "x"),
            ("1 5.5", ".5"),
            // Nothing converted at all, so the stop is the element's start.
            ("1 abc 5", "abc 5"),
            ("1 -", "-"),
        ] {
            let e = vector_in(input, VectorKind::Oid).unwrap_err();
            assert_eq!(e.sqlstate, "22P02", "input {input:?}");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type oid: \"{quoted}\"")
            );
        }
    }

    /// `int2vector` always quotes from the element's first character, even when
    /// a prefix of it converted.
    #[test]
    fn int2vector_syntax_errors_quote_from_the_element_start() {
        for (input, quoted) in [
            ("1 5x 7", "5x 7"),
            ("1 abc 5", "abc 5"),
            ("1 ,2", ",2"),
            ("1 5.5", "5.5"),
            ("1 -", "-"),
        ] {
            let e = vector_in(input, VectorKind::Int2).unwrap_err();
            assert_eq!(e.sqlstate, "22P02", "input {input:?}");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type smallint: \"{quoted}\"")
            );
        }
    }

    /// A magnitude the element type cannot hold is `22003`, quoted from the
    /// element's start — for both kinds, and whether the overflow is past the
    /// type's range or past `u64` entirely.
    #[test]
    fn out_of_range_elements_name_the_element_type() {
        for (input, quoted) in [
            ("01 9999999999", "9999999999"),
            ("1 4294967296", "4294967296"),
            ("1 9999999999 3", "9999999999 3"),
            ("1 18446744073709551616", "18446744073709551616"),
            ("1 -2147483649", "-2147483649"),
        ] {
            let e = vector_in(input, VectorKind::Oid).unwrap_err();
            assert_eq!(e.sqlstate, "22003", "input {input:?}");
            assert_eq!(
                e.message,
                format!("value \"{quoted}\" is out of range for type oid")
            );
        }
        for (input, quoted) in [("1 99999 3", "99999 3"), ("1 -32769", "-32769")] {
            let e = vector_in(input, VectorKind::Int2).unwrap_err();
            assert_eq!(e.sqlstate, "22003", "input {input:?}");
            assert_eq!(
                e.message,
                format!("value \"{quoted}\" is out of range for type smallint")
            );
        }
    }
}
