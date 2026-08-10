//! Validates `tsvector`/`tsquery` I/O and the `@@` match operator against the
//! statement/result pairs embedded in PostgreSQL's own `tstypes.out`.
//!
//! Rather than hand-transcribing expectations, this reads the vendored upstream
//! expected output and replays every assertion the types rung can answer:
//!
//! * `SELECT <lit>::tsvector;` / `::tsquery;` — canonical output text
//! * `SELECT <lit>::tsvector @@ <lit>;` — the match operator, including phrase
//!   search, prefixes, weights and negation
//!
//! TODO: replay the statements that need a text-search configuration
//! (`to_tsvector`) or ranking (`ts_rank`); neither exists yet, so they are
//! skipped here.

use crabgresql_types::tsquery::{self, TsQuery};
use crabgresql_types::tsvector::{self, TsVector};

const TSTYPES_OUT: &str = include_str!("../../../vendor/postgres/regress/expected/tstypes.out");

/// One replayable assertion from the upstream expected output.
enum Case {
    /// A cast whose canonical output text is pinned.
    Cast {
        input: String,
        kind: Kind,
        out: String,
    },
    /// A `tsvector @@ tsquery` assertion. `strip` mirrors an upstream
    /// `strip(...)` wrapper around the vector.
    Match {
        vector: String,
        strip: bool,
        query: String,
        expected: bool,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Vector,
    Query,
}

/// Decode one SQL string literal at `s[start..]`, returning the text and the
/// index just past the closing delimiter. Handles `'…'`, `E'…'` and `$$…$$`.
fn read_literal(s: &str, start: usize) -> Option<(String, usize)> {
    let b: Vec<char> = s.chars().collect();
    let mut i = start;
    while i < b.len() && b[i] == ' ' {
        i += 1;
    }
    if b.get(i) == Some(&'$') && b.get(i + 1) == Some(&'$') {
        let rest: String = b[i + 2..].iter().collect();
        let end = rest.find("$$")?;
        return Some((rest[..end].to_string(), i + 2 + end + 2));
    }
    let escaped = b.get(i) == Some(&'E') && b.get(i + 1) == Some(&'\'');
    if escaped {
        i += 1;
    }
    if b.get(i) != Some(&'\'') {
        return None;
    }
    i += 1;
    let mut out = String::new();
    while i < b.len() {
        match b[i] {
            '\'' if b.get(i + 1) == Some(&'\'') => {
                out.push('\'');
                i += 2;
            }
            '\'' => return Some((out, i + 1)),
            // Inside an E'' string a backslash escapes the next character.
            '\\' if escaped => {
                out.push(*b.get(i + 1)?);
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    None
}

/// The single scalar a one-row result block holds, or `None` if the statement
/// errored or returned something else.
fn single_value(lines: &[&str], stmt_idx: usize) -> Option<String> {
    // header, separator, value, "(1 row)"
    let value = lines.get(stmt_idx + 3)?;
    if lines.get(stmt_idx + 4)?.trim() != "(1 row)" {
        return None;
    }
    if !lines.get(stmt_idx + 2)?.trim_start().starts_with('-') {
        return None;
    }
    Some(value.trim().to_string())
}

fn collect_cases() -> Vec<Case> {
    let lines: Vec<&str> = TSTYPES_OUT.lines().collect();
    let mut cases = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("SELECT ") else {
            continue;
        };
        if !line.ends_with(';') {
            continue;
        }
        // TODO: replay the `to_tsvector`/`ts_rank` statements too; they need a
        // text-search configuration and ranking, which are not implemented.
        // `tsvectorin(tsvectorout(...))` nests a call this extractor does not
        // parse.
        if rest.contains("to_tsvector")
            || rest.contains("ts_rank")
            || rest.contains("tsvectorin")
            || rest.contains("pg_input")
        {
            continue;
        }
        let Some(out) = single_value(&lines, i) else {
            continue;
        };

        // `strip(<lit>::tsvector) @@ <lit>` or `<lit>::tsvector @@ <lit>`.
        let (body, strip) = match rest.strip_prefix("strip(") {
            Some(inner) => (inner, true),
            None => (rest, false),
        };
        let Some((vector, after)) = read_literal(body, 0) else {
            continue;
        };
        let tail: String = body.chars().skip(after).collect();

        if let Some(pos) = tail.find("@@") {
            // Reject anything between the literal and `@@` other than the cast
            // and (for the strip form) its closing paren.
            let between = tail[..pos].replace("::tsvector", "").replace(')', "");
            if !between.trim().is_empty() {
                continue;
            }
            let Some((query, _)) = read_literal(&tail[pos + 2..], 0) else {
                continue;
            };
            let expected = match out.as_str() {
                "t" => true,
                "f" => false,
                _ => continue,
            };
            cases.push(Case::Match {
                vector,
                strip,
                query,
                expected,
            });
            continue;
        }

        if strip {
            continue;
        }
        let kind = if tail.trim() == "::tsvector;" {
            Kind::Vector
        } else if tail.trim() == "::tsquery;" {
            Kind::Query
        } else {
            continue;
        };
        cases.push(Case::Cast {
            input: vector,
            kind,
            out,
        });
    }
    cases
}

fn parse_vector(s: &str) -> anyhow::Result<TsVector> {
    tsvector::tsvector_in(s).map_err(|e| anyhow::anyhow!("{}: {}", s, e.message))
}

fn parse_query(s: &str) -> anyhow::Result<TsQuery> {
    tsquery::tsquery_in(s).map_err(|e| anyhow::anyhow!("{}: {}", s, e.message))
}

#[test]
fn tstypes_casts_match_upstream_output() -> anyhow::Result<()> {
    let cases = collect_cases();
    let mut checked = 0;
    for case in &cases {
        let Case::Cast { input, kind, out } = case else {
            continue;
        };
        let got = match kind {
            Kind::Vector => tsvector::format(&parse_vector(input)?),
            Kind::Query => tsquery::format(&parse_query(input)?),
        };
        assert_eq!(&got, out, "input {input:?}");
        checked += 1;
    }
    assert!(checked >= 58, "expected a large cast corpus, got {checked}");
    Ok(())
}

#[test]
fn tstypes_match_operator_agrees_with_upstream() -> anyhow::Result<()> {
    let cases = collect_cases();
    let mut checked = 0;
    for case in &cases {
        let Case::Match {
            vector,
            strip,
            query,
            expected,
        } = case
        else {
            continue;
        };
        let mut tv = parse_vector(vector)?;
        if *strip {
            tv = tsvector::strip(&tv);
        }
        let q = parse_query(query)?;
        assert_eq!(
            tsquery::matches(&tv, &q),
            *expected,
            "{vector:?} (strip={strip}) @@ {query:?}"
        );
        checked += 1;
    }
    // Guards against the extractor silently going blind (e.g. an upstream
    // formatting change); the rest of the file's `@@` assertions wrap the
    // vector in `to_tsvector`, which `collect_cases` filters out.
    assert!(
        checked >= 37,
        "expected a large match corpus, got {checked}"
    );
    Ok(())
}
