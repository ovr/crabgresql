//! Randomized round-trip and robustness cover for the `jsonpath` storage codec,
//! which defines what a stored `jsonpath` decodes back to. A silent change here
//! is data corruption, so both directions are exercised over a generated corpus
//! rather than a handful of literals.
//!
//! The codec exists because the canonical text form is *not* a safe storage
//! format: `jsonpath_out` parenthesizes equal-priority sub-expressions, so a
//! path can re-parse deeper than it was written, and tightening the parser
//! retroactively makes stored values unreadable.

use crabgresql_types::jsonpath;

/// Deterministic LCG, so a failure reproduces exactly.
struct R(u64);

impl R {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

/// Build a syntactically valid jsonpath, biased towards the shapes the codec
/// has to carry: every accessor kind, both literal families, and the nodes with
/// payloads (`Var`, `LitNum`, `LikeRegex`, `Recursive`, `Subscript`).
fn build(r: &mut R, depth: usize) -> String {
    if depth == 0 {
        return match r.next(6) {
            0 => "$".to_string(),
            1 => "@".to_string(),
            2 => "1.5".to_string(),
            3 => "\"str\"".to_string(),
            4 => "true".to_string(),
            _ => "null".to_string(),
        };
    }
    let base = build(r, depth - 1);
    match r.next(14) {
        0 => format!("{base}.\"key\""),
        1 => format!("{base}.*"),
        2 => format!("{base}[*]"),
        3 => format!("{base}.**"),
        4 => format!("{base}.**{{2}}"),
        5 => format!("{base}.**{{1 to 3}}"),
        6 => format!("{base}[0, 1 to 2]"),
        7 => format!("{base}.size()"),
        8 => format!("{base}.abs()"),
        9 => format!("({base} + 2)"),
        10 => format!("({base} == 1 && @.x > 2)"),
        11 => format!("{base} ? (@ like_regex \"a.b\" flag \"imq\")"),
        12 => format!("{base} ? (exists (@.y) || !(@.z < 3))"),
        _ => format!("{base} ? (@ starts with \"p\")"),
    }
}

#[test]
fn codec_round_trips_over_a_generated_corpus() {
    let mut r = R(0x5eed);
    let mut checked = 0;
    for _ in 0..2000 {
        let depth = r.next(4) + 1;
        let src = build(&mut r, depth);
        let src = if r.next(2) == 0 {
            format!("strict {src}")
        } else {
            src
        };
        let Ok(path) = jsonpath::jsonpath_in(&src) else {
            continue;
        };
        let decoded = jsonpath::decode(&jsonpath::encode(&path))
            .unwrap_or_else(|| panic!("decode failed for {src:?}"));
        // Equality is structural, so this pins the tree, not just its spelling.
        assert_eq!(decoded, path, "tree changed for {src:?}");
        assert_eq!(
            jsonpath::format(&decoded),
            jsonpath::format(&path),
            "output changed for {src:?}"
        );
        checked += 1;
    }
    assert!(checked > 500, "corpus too small: {checked}");
}

/// A corrupt page must not panic or hang the backend.
#[test]
fn decode_rejects_garbage_without_panicking() {
    let path = jsonpath::jsonpath_in("$.\"a\"[*] ? (@ > 3)").expect("valid");
    let good = jsonpath::encode(&path);

    // Truncations.
    for n in 0..good.len() {
        let _ = jsonpath::decode(&good[..n]);
    }
    // Trailing bytes are rejected rather than ignored.
    let mut extra = good.clone();
    extra.push(0);
    assert_eq!(jsonpath::decode(&extra), None);
    // Single-byte corruptions.
    let mut r = R(1);
    for _ in 0..3000 {
        let mut bad = good.clone();
        let i = r.next(bad.len());
        bad[i] = r.next(256) as u8;
        let _ = jsonpath::decode(&bad);
    }
    // Unknown tags and an empty buffer.
    assert_eq!(jsonpath::decode(&[]), None);
    assert_eq!(jsonpath::decode(&[0, 250]), None);
    // A deeply nested tree must be refused rather than overflow the stack.
    let deep: Vec<u8> = std::iter::once(0u8).chain(std::iter::repeat_n(13, 5000)).collect();
    assert_eq!(jsonpath::decode(&deep), None);
}
