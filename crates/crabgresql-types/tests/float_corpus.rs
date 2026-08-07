//! Validates `float::fmt_f32`/`fmt_f64` against the exact bit-pattern → text
//! corpus embedded in PostgreSQL's `float4.out`/`float8.out`. Each round-trip
//! row is `ibits | flt [| ...]` where `ibits` is `floatNsend(flt)` (the IEEE
//! bytes) and `flt` is that value rendered at the default extra_float_digits.

use crabgresql_types::float::{fmt_f32, fmt_f64};

const FLOAT4_OUT: &str = include_str!("../../../vendor/postgres/regress/expected/float4.out");
const FLOAT8_OUT: &str = include_str!("../../../vendor/postgres/regress/expected/float8.out");

/// Extract `(hex, expected_text)` pairs from data rows whose first column is a
/// bytea of exactly `hex_len` hex digits.
fn corpus(out: &str, hex_len: usize) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in out.lines() {
        let Some(rest) = line.trim_start().strip_prefix("\\x") else {
            continue;
        };
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 2 {
            continue;
        }
        let hex = &rest[..rest
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(rest.len())];
        if hex.len() != hex_len {
            continue;
        }
        pairs.push((hex.to_string(), cols[1].to_string()));
    }
    pairs
}

#[test]
fn float4_corpus_matches() -> anyhow::Result<()> {
    let pairs = corpus(FLOAT4_OUT, 8);
    assert!(
        pairs.len() > 200,
        "expected a large corpus, got {}",
        pairs.len()
    );
    for (hex, expected) in pairs {
        let bits = u32::from_str_radix(&hex, 16)?;
        let got = fmt_f32(f32::from_bits(bits), 1);
        assert_eq!(got, expected, "f32 from bits 0x{hex}");
    }

    Ok(())
}

#[test]
fn float8_corpus_matches() -> anyhow::Result<()> {
    let pairs = corpus(FLOAT8_OUT, 16);
    assert!(
        pairs.len() > 200,
        "expected a large corpus, got {}",
        pairs.len()
    );
    for (hex, expected) in pairs {
        let bits = u64::from_str_radix(&hex, 16)?;
        let got = fmt_f64(f64::from_bits(bits), 1);
        assert_eq!(got, expected, "f64 from bits 0x{hex}");
    }

    Ok(())
}
