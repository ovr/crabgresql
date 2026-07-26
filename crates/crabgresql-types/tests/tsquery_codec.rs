//! Randomized round-trip and robustness cover for the `tsquery` storage codec
//! and the `tsvector` canonical-text codec, which together define what a stored
//! text-search value decodes back to. A silent change here is data corruption,
//! so both directions are exercised over a generated corpus rather than a
//! handful of literals.

use crabgresql_types::tsquery::{self, TsQuery};
use crabgresql_types::tsvector;

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

fn build_q(r: &mut R, depth: usize) -> String {
    let words = ["a", "bb", "c'c", "d\\d", "ee", "x"];
    if depth == 0 || r.next(3) == 0 {
        let w = words[r.next(words.len())];
        let mut s = format!("'{}'", w.replace('\'', "''"));
        match r.next(4) {
            0 => s.push_str(":*"),
            1 => s.push_str(":AB"),
            2 => s.push_str(":*D"),
            _ => {}
        }
        return s;
    }
    let l = build_q(r, depth - 1);
    let rr = build_q(r, depth - 1);
    match r.next(5) {
        0 => format!("!({l})"),
        1 => format!("({l} & {rr})"),
        2 => format!("({l} | {rr})"),
        3 => format!("({l} <-> {rr})"),
        _ => format!("({l} <{}> {rr})", r.next(17)),
    }
}

#[test]
fn tsquery_codec_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = R(42);
    let mut n = 0;
    for _ in 0..4000 {
        let src = build_q(&mut r, 4);
        let Ok(q) = tsquery::tsquery_in(&src) else { continue };
        let bytes = tsquery::encode(&q);
        let back = tsquery::decode(&bytes).ok_or("decode failed")?;
        // The tree itself must survive, not just its printed form -- `&`/`|`
        // associativity is invisible to `tsquery_out`.
        assert_eq!(q, back, "tree changed for {src}");
        assert_eq!(tsquery::format(&q), tsquery::format(&back));
        n += 1;
    }
    assert!(n > 3000, "only {n} usable cases");
    // empty query
    // The empty query is storable too.
    let e = TsQuery::default();
    assert_eq!(tsquery::decode(&tsquery::encode(&e)).ok_or("decode failed")?, e);
    Ok(())
}

#[test]
fn tsquery_decode_rejects_garbage_without_panicking() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = R(7);
    let good = tsquery::encode(&tsquery::tsquery_in("('a' & !'b') <2> 'c':*AB")?);
    for _ in 0..20000 {
        let mut b = good.clone();
        if !b.is_empty() {
            let i = r.next(b.len());
            b[i] = (r.next(256)) as u8;          // corrupt a byte
        }
        let cut = r.next(b.len() + 1);
        b.truncate(cut);                          // and/or truncate
        let _ = tsquery::decode(&b);              // must not panic
    }
    for len in 0..40usize {
        let b: Vec<u8> = (0..len).map(|i| (r.next(256) ^ i) as u8).collect();
        let _ = tsquery::decode(&b);
    }
    Ok(())
}

#[test]
fn tsvector_text_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = R(99);
    let mut n = 0;
    for _ in 0..3000 {
        let mut parts = Vec::new();
        for _ in 0..(1 + r.next(4)) {
            let w = ["a", "bb", "c'c", "d\\d", "zz"][r.next(5)];
            let mut p = format!("'{}'", w.replace('\'', "''"));
            if r.next(2) == 0 {
                let k = 1 + r.next(3);
                let ps: Vec<String> = (0..k).map(|_| format!("{}{}", 1 + r.next(200), ["", "A", "B", "C"][r.next(4)])).collect();
                p.push(':');
                p.push_str(&ps.join(","));
            }
            parts.push(p);
        }
        let src = parts.join(" ");
        let Ok(v) = tsvector::tsvector_in(&src) else { continue };
        let text = tsvector::format(&v);
        let back = tsvector::tsvector_in(&text)?;
        assert_eq!(v, back, "tsvector changed for {src} -> {text}");
        n += 1;
    }
    assert!(n > 2500, "only {n} usable cases");
    Ok(())
}

/// Equality follows PostgreSQL: a leaf's prefix flag and weight mask never
/// separate two otherwise-equal queries, so the storage codec must still keep
/// them (the value that comes back out has to *print* the same).
#[test]
fn codec_preserves_what_equality_ignores() -> Result<(), Box<dyn std::error::Error>> {
    for src in ["'a'", "'a':*", "'a':AB", "'a':*D"] {
        let q = tsquery::tsquery_in(src)?;
        let back = tsquery::decode(&tsquery::encode(&q)).ok_or("decode failed")?;
        assert_eq!(tsquery::format(&back), src, "lost detail for {src}");
    }
    // ... even though all four compare equal.
    let a = tsquery::tsquery_in("'a'")?;
    for other in ["'a':*", "'a':AB", "'a':*D"] {
        assert_eq!(tsquery::cmp(&a, &tsquery::tsquery_in(other)?), std::cmp::Ordering::Equal);
    }
    Ok(())
}
