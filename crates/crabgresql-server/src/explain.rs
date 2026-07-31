//! `EXPLAIN` option handling and the `ANALYZE` run.
//!
//! The plan text itself is rendered by `crabgresql_planner::explain`; this module
//! decides *what* to render (which modifiers the statement asked for) and, under
//! `ANALYZE`, actually runs the statement and times it.

use std::time::{Duration, Instant};

use crabgresql_executor::{
    ExecContext, ExecError, ExecNode, Execution, OutputColumn, Values, execute,
};
use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_planner::PhysicalPlan;
use crabgresql_txn::TxnContext;
use crabgresql_types::{PgType, Value};

use crate::error::PgError;
use crate::query::{QueryResult, RowTag, normalize_ident};
use crate::session::Session;

/// The `EXPLAIN` modifiers crabgresql acts on. Everything else is either
/// accepted and ignored (because PG's own default already differs from our
/// reduced output) or rejected by [`ExplainOptions::resolve`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExplainOptions {
    /// Run the statement and report its execution, not just its plan.
    pub analyze: bool,
    /// Append the `Planning Time:` / `Execution Time:` footers.
    pub summary: bool,
}

impl ExplainOptions {
    /// Resolve the modifiers from both spellings the parser produces: the bare
    /// `EXPLAIN ANALYZE VERBOSE FORMAT TEXT` form fills the `analyze`/`verbose`/
    /// `format` fields of the AST node, while the parenthesized
    /// `EXPLAIN (ANALYZE, TIMING OFF)` form lands entirely in `options` as
    /// generic name/value pairs.
    ///
    /// An option that would change the *shape* of the output we cannot produce
    /// is rejected rather than ignored — EXPLAIN output is part of the
    /// compatibility surface, so answering `FORMAT JSON` with text is worse than
    /// admitting the gap. `COSTS` and `BUFFERS` are accepted and ignored: our
    /// output is permanently `COSTS OFF` shaped (there is no cost model), and PG
    /// turns `BUFFERS` on by default under `ANALYZE`, so rejecting the explicit
    /// spelling while silently omitting the implicit default would be incoherent.
    pub fn resolve(
        analyze: bool,
        verbose: bool,
        options: Option<&[ast::UtilityOption]>,
    ) -> Result<Self, PgError> {
        let mut analyze = analyze;
        let mut summary = None;
        // Every flag is *last wins*, as in PG: it applies the option list in order,
        // so `EXPLAIN (MEMORY ON, MEMORY OFF)` is accepted — the final value is off.
        // Nothing may be rejected while the list is still being read, or a later
        // `OFF` could not take an earlier `ON` back.
        let mut verbose = verbose;
        let mut timing = false;
        let mut wal = false;
        let mut serialize = false;
        let mut settings = false;
        let mut memory = false;
        let mut generic_plan = false;
        // `FORMAT <name>`, kept as written: PG compares it exactly, so only the
        // lexer's folding of an unquoted word makes `FORMAT TEXT` work while
        // `FORMAT 'TEXT'` does not.
        let mut format: Option<String> = None;
        for option in options.unwrap_or_default() {
            // The option name is an identifier, so it folds to lowercase unless it
            // was quoted — which is also the spelling PG echoes back in the errors
            // below (`unrecognized EXPLAIN option "bogus"` for `(BOGUS)`).
            let name = normalize_ident(&option.name);
            match name.as_str() {
                // PG's grammar folds the British spelling to `analyze` before the
                // value is read, so the error echoes "analyze" either way.
                "analyze" | "analyse" => analyze = option_flag(option, "analyze")?,
                "summary" => summary = Some(option_flag(option, &name)?),
                // TIMING needs ANALYZE but asks for nothing we cannot deliver: it
                // is validated so `EXPLAIN (ANALYZE, TIMING OFF)` is accepted today
                // and keeps meaning the same thing once per-node times arrive.
                "timing" => timing = option_flag(option, &name)?,
                // Accepted and ignored: neither changes which lines we print.
                "costs" | "buffers" => {
                    option_flag(option, &name)?;
                }
                // Recognized by PG, and shape-changing when switched on: VERBOSE
                // adds `Output:` lines, SETTINGS a `Settings:` line, MEMORY a
                // `Planning:`/`Memory:` block, WAL a `WAL:` line, and GENERIC_PLAN
                // plans the statement differently.
                "verbose" => verbose = option_flag(option, &name)?,
                "settings" => settings = option_flag(option, &name)?,
                "memory" => memory = option_flag(option, &name)?,
                "wal" => wal = option_flag(option, &name)?,
                "generic_plan" => generic_plan = option_flag(option, &name)?,
                // SERIALIZE takes a mode, not a boolean, and defaults to `text` when
                // bare. `none` and `off` are the two spellings that ask for nothing.
                "serialize" => {
                    let mode = match option.arg {
                        None => "text".to_string(),
                        Some(_) => option_text(option, &name)?,
                    };
                    serialize = match mode.as_str() {
                        "none" | "off" => false,
                        "text" | "binary" => true,
                        _ => return Err(unrecognized_value(&name, &mode)),
                    };
                }
                // The format is checked after the loop, where PG's ordering puts it
                // relative to the cross-checks.
                "format" => format = Some(option_text(option, &name)?),
                _ => {
                    return Err(PgError::new(
                        sqlstate::SYNTAX_ERROR,
                        format!("unrecognized EXPLAIN option \"{name}\""),
                    ));
                }
            }
        }
        // From here the options are settled, and the checks run in PG's order —
        // verified against PG 18.4 to be independent of the order they were
        // written in. An unusable FORMAT value comes first, because PG validates it
        // as it reads the option rather than in a cross-check. The comparison is
        // exact: `FORMAT TEXT` works only because the lexer folds an unquoted word,
        // while `FORMAT 'TEXT'` is an unrecognized value.
        let format = match format.as_deref() {
            None | Some("text") => None,
            Some("json" | "xml" | "yaml") => format,
            Some(other) => return Err(unrecognized_value("format", other)),
        };
        // Then the cross-checks: `EXPLAIN (TIMING ON)` reports the dependency, not
        // a missing feature. PG picks WAL over TIMING over SERIALIZE.
        if !analyze {
            for (requested, option) in [(wal, "WAL"), (timing, "TIMING"), (serialize, "SERIALIZE")] {
                if requested {
                    return Err(PgError::new(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format!("EXPLAIN option {option} requires ANALYZE"),
                    ));
                }
            }
        }
        // GENERIC_PLAN asks for a plan built without the parameter values ANALYZE
        // needs in order to run, so PG rejects the combination outright.
        if generic_plan && analyze {
            return Err(PgError::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together",
            ));
        }
        // Last, the options PG honors and crabgresql cannot yet produce.
        if let Some(format) = format {
            return Err(unsupported_option(&format!(
                "FORMAT {}",
                format.to_ascii_uppercase()
            )));
        }
        for (requested, option) in [
            (verbose, "VERBOSE"),
            (settings, "SETTINGS"),
            (memory, "MEMORY"),
            (wal, "WAL"),
            (serialize, "SERIALIZE"),
            (generic_plan, "GENERIC_PLAN"),
        ] {
            if requested {
                return Err(unsupported_option(option));
            }
        }
        Ok(Self {
            analyze,
            // PG defaults SUMMARY to ANALYZE: the footers report execution, so
            // they only appear when the statement ran.
            summary: summary.unwrap_or(analyze),
        })
    }
}

/// An EXPLAIN modifier crabgresql parses but cannot honor. PG supports these, so
/// the gap is ours to report rather than to paper over.
fn unsupported_option(option: &str) -> PgError {
    PgError::feature_not_supported(format!("EXPLAIN ({option}) is not supported yet"))
}

/// A value-taking option given a word it does not know, e.g.
/// `EXPLAIN (FORMAT bogus)`. `value` is echoed exactly as the client wrote it.
fn unrecognized_value(option: &str, value: &str) -> PgError {
    PgError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("unrecognized value for EXPLAIN option \"{option}\": \"{value}\""),
    )
}

/// The word a value-taking option carries, with PG's identifier folding applied:
/// an unquoted word lowercases (so `FORMAT TEXT` means `text`), while a quoted or
/// string literal keeps its case (so `FORMAT 'TEXT'` stays `TEXT`, and is
/// therefore *not* the `text` format). Errors if the option carries no value, or
/// one that is not a word or literal — reporting it the way PG does either way.
fn option_text(option: &ast::UtilityOption, name: &str) -> Result<String, PgError> {
    if option.arg.is_none() {
        // `EXPLAIN (FORMAT) SELECT 1` is
        // `ERROR: 42601: format requires a parameter` in PG.
        return Err(PgError::syntax(format!("{name} requires a parameter")));
    }
    // An argument that is not a word or a scalar literal — `-1`, `NULL`, `1+1` —
    // names no value, and PG echoes it as the unrecognized value it is.
    option_value(option).ok_or_else(|| {
        let written = option.arg.as_ref().map_or(String::new(), ToString::to_string);
        unrecognized_value(name, &written)
    })
}

/// The boolean an option carries: a bare option name is TRUE, and only
/// `on`/`off`, `true`/`false` and `1`/`0` set it explicitly (case-insensitively).
/// `name` is the folded option name the error echoes.
///
/// The accepted set is deliberately narrower than [`crabgresql_types::parse_bool`],
/// which serves `SET` and the text→boolean cast: `SET x = tr` and `SELECT 'y'::bool`
/// are valid, while `EXPLAIN (COSTS tr)` and `EXPLAIN (COSTS y)` are
/// `ERROR: 42601: costs requires a Boolean value` in PG. Do not unify the two.
fn option_flag(option: &ast::UtilityOption, name: &str) -> Result<bool, PgError> {
    // Only a *missing* argument means TRUE. An argument that is present but not a
    // boolean — `(ANALYZE -1)`, `(ANALYZE NULL)`, `(SUMMARY 1+1)` — must be
    // rejected, never silently read as TRUE: reading it as TRUE would run the
    // statement, so a typo in what looks like a read-only EXPLAIN would write.
    if option.arg.is_none() {
        return Ok(true);
    }
    let invalid = || {
        PgError::new(
            sqlstate::SYNTAX_ERROR,
            format!("{name} requires a Boolean value"),
        )
    };
    match option_value(option).ok_or_else(invalid)?.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(invalid()),
    }
}

/// The bare word, string or number an option carries, e.g. `FORMAT json`,
/// `FORMAT 'json'`, `FORMAT $$json$$` (all three accepted by PG) or `ANALYZE off`.
/// A bare word folds like any identifier; a literal is returned verbatim.
/// `None` when there is no argument *or* the argument is not a word or scalar
/// literal — callers must distinguish those two cases themselves via `option.arg`,
/// because an unreadable value has to be an error, not a default.
fn option_value(option: &ast::UtilityOption) -> Option<String> {
    match option.arg.as_ref()? {
        ast::Expr::Identifier(ident) => Some(normalize_ident(ident)),
        ast::Expr::Value(value) => match &value.value {
            ast::Value::Number(n, _) => Some(n.to_string()),
            ast::Value::Boolean(b) => Some(b.to_string()),
            other => other.as_pg_string().map(str::to_string),
        },
        _ => None,
    }
}

/// Run `plan` for `EXPLAIN ANALYZE`: execute it in the statement's transaction
/// and drain the result to completion, discarding the rows — as in PG, the client
/// receives the plan text, not the statement's own output. Returns the elapsed
/// time the `Execution Time:` footer reports.
///
/// The drain belongs here, before the caller commits: our nodes are lazy, so an
/// undrained result set would execute (and measure) nothing, and a fault raised
/// mid-stream — a projection dividing by zero, a `RETURNING` cast failure — must
/// abort the statement rather than surface after its commit.
pub fn run_analyze(
    plan: PhysicalPlan,
    ctx: &ExecContext,
    txn: &TxnContext,
) -> Result<Duration, ExecError> {
    let started = Instant::now();
    match execute(plan, ctx, txn)? {
        Execution::Rows { mut node, .. } | Execution::ReturningRows { mut node, .. } => {
            while node.next()?.is_some() {}
        }
        // A non-RETURNING mutation has already applied every row by the time
        // `execute` returns; there is nothing left to pull.
        Execution::Inserted(_) | Execution::Updated(_) | Execution::Deleted(_) => {}
    }
    Ok(started.elapsed())
}

/// Package plan text as the single-column `QUERY PLAN` result set EXPLAIN
/// returns, one row per line.
pub fn explain_result(lines: Vec<String>, session: &Session) -> QueryResult {
    let rows = lines
        .into_iter()
        .map(|line| {
            vec![crabgresql_binder::BoundExpr::Const {
                value: Value::Text(line),
                ty: PgType::Text,
            }]
        })
        .collect();
    let node: Box<dyn ExecNode> = Box::new(Values::new(rows, session.exec_context()));
    QueryResult::Rows {
        columns: explain_columns(),
        node,
        tag: RowTag::Explain,
        notices: Vec::new(),
    }
}

/// The result shape every EXPLAIN returns. Shared with the `Describe` path so the
/// RowDescription it advertises cannot drift from the DataRows that follow.
pub fn explain_columns() -> Vec<OutputColumn> {
    vec![OutputColumn::new("QUERY PLAN", PgType::Text)]
}

#[cfg(test)]
mod tests {
    //! SQL in, [`ExplainOptions`] (or a SQLSTATE) out — driven through the real
    //! parser so both option spellings are covered as a client would write them.

    use super::*;

    /// Resolve the options of an `EXPLAIN` statement written as SQL. The outer
    /// `anyhow::Result` reports a parse failure (a broken test), the inner
    /// `Result` the option rejection under test.
    fn resolve(sql: &str) -> anyhow::Result<Result<ExplainOptions, PgError>> {
        let statements = crabgresql_parser::parse(sql)?;
        let [
            ast::Statement::Explain {
                analyze,
                verbose,
                options,
                ..
            },
        ] = statements.as_slice()
        else {
            anyhow::bail!("expected one EXPLAIN statement, got: {statements:?}");
        };
        Ok(ExplainOptions::resolve(
            *analyze,
            *verbose,
            options.as_deref(),
        ))
    }

    /// The options of an `EXPLAIN` that must be accepted.
    fn accept(sql: &str) -> anyhow::Result<ExplainOptions> {
        resolve(sql)?.map_err(|e| anyhow::anyhow!("{sql}: {} {}", e.code, e.message))
    }

    /// The SQLSTATE and message of an `EXPLAIN` that must be rejected.
    fn reject(sql: &str) -> anyhow::Result<(&'static str, String)> {
        let err = resolve(sql)?.err().ok_or_else(|| {
            anyhow::anyhow!("{sql}: expected the options to be rejected, they were accepted")
        })?;
        // Every EXPLAIN option error is built from a `sqlstate` constant, so the
        // code is always borrowed. Only a code composed at runtime (`RAISE ...
        // USING ERRCODE`) is owned, and this path never builds one.
        let code = match err.code {
            std::borrow::Cow::Borrowed(code) => code,
            std::borrow::Cow::Owned(code) => {
                anyhow::bail!("{sql}: expected a static SQLSTATE, got {code}")
            }
        };
        Ok((code, err.message))
    }

    #[test]
    fn plain_explain_neither_analyzes_nor_summarizes() -> anyhow::Result<()> {
        assert_eq!(
            accept("EXPLAIN SELECT 1")?,
            ExplainOptions {
                analyze: false,
                summary: false
            }
        );
        Ok(())
    }

    #[test]
    fn analyze_defaults_summary_on() -> anyhow::Result<()> {
        // Every spelling PG accepts, including the two the parser routes
        // differently (bare keyword vs. parenthesized option list).
        for sql in [
            "EXPLAIN ANALYZE SELECT 1",
            "EXPLAIN (ANALYZE) SELECT 1",
            "EXPLAIN (analyze true) SELECT 1",
            "EXPLAIN (analyze 1) SELECT 1",
            "EXPLAIN (ANALYZE on) SELECT 1",
        ] {
            assert_eq!(
                accept(sql)?,
                ExplainOptions {
                    analyze: true,
                    summary: true
                },
                "{sql}"
            );
        }
        Ok(())
    }

    #[test]
    fn analyze_off_is_a_plain_explain() -> anyhow::Result<()> {
        let opts = accept("EXPLAIN (ANALYZE off) SELECT 1")?;
        assert!(!opts.analyze);
        assert!(!opts.summary);
        Ok(())
    }

    #[test]
    fn summary_off_suppresses_the_footers() -> anyhow::Result<()> {
        let opts = accept("EXPLAIN (ANALYZE, SUMMARY OFF) SELECT 1")?;
        assert!(opts.analyze);
        assert!(!opts.summary);
        Ok(())
    }

    #[test]
    fn summary_on_without_analyze_is_accepted() -> anyhow::Result<()> {
        // PG prints the planning footer for a non-ANALYZE EXPLAIN when asked, and
        // we do have a planning time to report — no rejection needed.
        let opts = accept("EXPLAIN (SUMMARY ON) SELECT 1")?;
        assert!(!opts.analyze);
        assert!(opts.summary);
        Ok(())
    }

    #[test]
    fn timing_is_accepted_under_analyze() -> anyhow::Result<()> {
        for sql in [
            "EXPLAIN (TIMING OFF) SELECT 1",
            "EXPLAIN (ANALYZE, TIMING OFF) SELECT 1",
            "EXPLAIN (ANALYZE, TIMING ON) SELECT 1",
        ] {
            accept(sql)?;
        }
        Ok(())
    }

    #[test]
    fn costs_and_buffers_are_accepted_and_ignored() -> anyhow::Result<()> {
        for sql in [
            "EXPLAIN (COSTS OFF) SELECT 1",
            "EXPLAIN (COSTS ON) SELECT 1",
            "EXPLAIN (BUFFERS) SELECT 1",
            "EXPLAIN (ANALYZE, BUFFERS OFF, COSTS OFF) SELECT 1",
        ] {
            accept(sql)?;
        }
        Ok(())
    }

    #[test]
    fn text_format_is_the_only_supported_format() -> anyhow::Result<()> {
        // Only the parenthesized spelling reaches here; the bare
        // `EXPLAIN FORMAT <kind>` form is not PG grammar and is rejected upstream.
        accept("EXPLAIN (FORMAT TEXT) SELECT 1")?;
        for (sql, option) in [
            ("EXPLAIN (FORMAT JSON) SELECT 1", "FORMAT JSON"),
            ("EXPLAIN (FORMAT xml) SELECT 1", "FORMAT XML"),
            ("EXPLAIN (FORMAT yaml) SELECT 1", "FORMAT YAML"),
        ] {
            assert_eq!(
                reject(sql)?,
                (
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format!("EXPLAIN ({option}) is not supported yet")
                ),
                "{sql}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_format_value_is_compared_exactly() -> anyhow::Result<()> {
        // PG compares the format name with `strcmp`, so only the lexer's folding of
        // an unquoted word makes the upper-case spelling work.
        accept("EXPLAIN (FORMAT TEXT) SELECT 1")?;
        accept("EXPLAIN (FORMAT 'text') SELECT 1")?;
        for sql in [
            "EXPLAIN (FORMAT 'TEXT') SELECT 1",
            "EXPLAIN (FORMAT \"TEXT\") SELECT 1",
        ] {
            assert_eq!(
                reject(sql)?,
                (
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "unrecognized value for EXPLAIN option \"format\": \"TEXT\"".to_string()
                ),
                "{sql}"
            );
        }
        // A quoted JSON is likewise an unrecognized *value*, not a missing feature.
        assert_eq!(
            reject("EXPLAIN (FORMAT 'JSON') SELECT 1")?.0,
            sqlstate::INVALID_PARAMETER_VALUE
        );
        // Formats PG has no name for at all.
        for name in ["graphviz", "tree", "traditional", "bogus"] {
            assert_eq!(
                reject(&format!("EXPLAIN (FORMAT {name}) SELECT 1"))?,
                (
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!("unrecognized value for EXPLAIN option \"format\": \"{name}\"")
                ),
                "FORMAT {name}"
            );
        }
        Ok(())
    }

    #[test]
    fn serialize_takes_a_mode_and_validates_it() -> anyhow::Result<()> {
        // `none` and `off` both ask for nothing, so neither needs ANALYZE.
        accept("EXPLAIN (SERIALIZE none) SELECT 1")?;
        accept("EXPLAIN (SERIALIZE off) SELECT 1")?;
        accept("EXPLAIN (SERIALIZE NONE) SELECT 1")?;
        // An unknown mode is an unrecognized value, reported before the
        // requires-ANALYZE cross-check and regardless of ANALYZE.
        for sql in [
            "EXPLAIN (SERIALIZE bogus) SELECT 1",
            "EXPLAIN (ANALYZE, SERIALIZE bogus) SELECT 1",
        ] {
            assert_eq!(
                reject(sql)?,
                (
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "unrecognized value for EXPLAIN option \"serialize\": \"bogus\"".to_string()
                ),
                "{sql}"
            );
        }
        // Quoted, so compared exactly — `'NONE'` is not the `none` mode.
        assert_eq!(
            reject("EXPLAIN (SERIALIZE 'NONE') SELECT 1")?,
            (
                sqlstate::INVALID_PARAMETER_VALUE,
                "unrecognized value for EXPLAIN option \"serialize\": \"NONE\"".to_string()
            )
        );
        Ok(())
    }

    #[test]
    fn an_unreadable_value_is_echoed_as_written() -> anyhow::Result<()> {
        // PG renders the offending argument rather than dropping it, so the client
        // is told what was rejected.
        assert_eq!(
            reject("EXPLAIN (FORMAT -1) SELECT 1")?,
            (
                sqlstate::INVALID_PARAMETER_VALUE,
                "unrecognized value for EXPLAIN option \"format\": \"-1\"".to_string()
            )
        );
        Ok(())
    }

    #[test]
    fn an_unrecognized_format_value_reports_it_back() -> anyhow::Result<()> {
        assert_eq!(
            reject("EXPLAIN (FORMAT bogus) SELECT 1")?,
            (
                sqlstate::INVALID_PARAMETER_VALUE,
                "unrecognized value for EXPLAIN option \"format\": \"bogus\"".to_string()
            )
        );
        Ok(())
    }

    #[test]
    fn verbose_is_rejected_in_both_spellings() -> anyhow::Result<()> {
        for sql in [
            "EXPLAIN VERBOSE SELECT 1",
            "EXPLAIN (VERBOSE) SELECT 1",
            "EXPLAIN (VERBOSE TRUE) SELECT 1",
        ] {
            assert_eq!(
                reject(sql)?,
                (
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "EXPLAIN (VERBOSE) is not supported yet".to_string()
                ),
                "{sql}"
            );
        }
        // Explicitly off asks for nothing we cannot deliver.
        accept("EXPLAIN (VERBOSE OFF) SELECT 1")?;
        Ok(())
    }

    #[test]
    fn shape_changing_options_are_rejected_only_when_switched_on() -> anyhow::Result<()> {
        for (name, sql_on, sql_off) in [
            (
                "SETTINGS",
                "EXPLAIN (SETTINGS) SELECT 1",
                "EXPLAIN (SETTINGS OFF) SELECT 1",
            ),
            (
                "WAL",
                "EXPLAIN (ANALYZE, WAL) SELECT 1",
                "EXPLAIN (ANALYZE, WAL OFF) SELECT 1",
            ),
            (
                "MEMORY",
                "EXPLAIN (MEMORY) SELECT 1",
                "EXPLAIN (MEMORY OFF) SELECT 1",
            ),
            (
                "GENERIC_PLAN",
                "EXPLAIN (GENERIC_PLAN) SELECT 1",
                "EXPLAIN (GENERIC_PLAN OFF) SELECT 1",
            ),
        ] {
            assert_eq!(
                reject(sql_on)?,
                (
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format!("EXPLAIN ({name}) is not supported yet")
                ),
                "{sql_on}"
            );
            accept(sql_off)?;
        }
        // SERIALIZE takes a mode, not a boolean: only `none` is a no-op.
        accept("EXPLAIN (ANALYZE, SERIALIZE none) SELECT 1")?;
        assert_eq!(
            reject("EXPLAIN (ANALYZE, SERIALIZE) SELECT 1")?.0,
            sqlstate::FEATURE_NOT_SUPPORTED
        );
        assert_eq!(
            reject("EXPLAIN (ANALYZE, SERIALIZE text) SELECT 1")?.0,
            sqlstate::FEATURE_NOT_SUPPORTED
        );
        Ok(())
    }

    #[test]
    fn options_needing_analyze_report_the_dependency_before_the_gap() -> anyhow::Result<()> {
        // PG reads the whole option list, then cross-checks: WAL and SERIALIZE are
        // meaningless without ANALYZE, so that — not crabgresql's missing support —
        // is the error the client sees.
        for (option, sql) in [
            ("TIMING", "EXPLAIN (TIMING ON) SELECT 1"),
            ("WAL", "EXPLAIN (WAL) SELECT 1"),
            ("WAL", "EXPLAIN (TIMING OFF, WAL ON) SELECT 1"),
            ("SERIALIZE", "EXPLAIN (SERIALIZE) SELECT 1"),
            ("SERIALIZE", "EXPLAIN (SERIALIZE binary) SELECT 1"),
        ] {
            assert_eq!(
                reject(sql)?,
                (
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!("EXPLAIN option {option} requires ANALYZE")
                ),
                "{sql}"
            );
        }
        // Switched off, they need nothing.
        accept("EXPLAIN (WAL OFF) SELECT 1")?;
        accept("EXPLAIN (SERIALIZE none) SELECT 1")?;
        // An unrecognized name still wins: it is rejected while the list is read.
        assert_eq!(
            reject("EXPLAIN (WAL, BOGUS) SELECT 1")?.0,
            sqlstate::SYNTAX_ERROR
        );
        Ok(())
    }

    #[test]
    fn analyze_and_generic_plan_conflict() -> anyhow::Result<()> {
        // GENERIC_PLAN plans without the parameter values ANALYZE needs to run.
        assert_eq!(
            reject("EXPLAIN (ANALYZE, GENERIC_PLAN) SELECT 1")?,
            (
                sqlstate::INVALID_PARAMETER_VALUE,
                "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together".to_string()
            )
        );
        Ok(())
    }

    #[test]
    fn an_unreadable_option_value_never_defaults_to_true() -> anyhow::Result<()> {
        // The dangerous case: if `-1` were read as "no value given", ANALYZE would
        // turn on and `EXPLAIN (ANALYZE -1) DELETE FROM t` would delete rows.
        for sql in [
            "EXPLAIN (ANALYZE -1) SELECT 1",
            "EXPLAIN (ANALYZE NULL) SELECT 1",
            "EXPLAIN (SUMMARY 1+1) SELECT 1",
        ] {
            assert_eq!(reject(sql)?.0, sqlstate::SYNTAX_ERROR, "{sql}");
        }
        Ok(())
    }

    #[test]
    fn only_pgs_boolean_spellings_are_accepted() -> anyhow::Result<()> {
        for value in ["on", "off", "true", "false", "1", "0", "TRUE", "Off"] {
            accept(&format!("EXPLAIN (COSTS {value}) SELECT 1"))?;
        }
        // PG accepts none of these for a utility option, though `SET x = tr` and
        // `'y'::bool` do — the two boolean readers are deliberately different.
        for value in ["t", "f", "yes", "no", "y", "n", "tr", "2"] {
            assert_eq!(
                reject(&format!("EXPLAIN (COSTS {value}) SELECT 1"))?,
                (
                    sqlstate::SYNTAX_ERROR,
                    "costs requires a Boolean value".to_string()
                ),
                "COSTS {value}"
            );
        }
        Ok(())
    }

    #[test]
    fn an_option_value_may_be_quoted_or_dollar_quoted() -> anyhow::Result<()> {
        // All three spellings PG accepts for a value-taking option.
        for sql in [
            "EXPLAIN (FORMAT text) SELECT 1",
            "EXPLAIN (FORMAT 'text') SELECT 1",
            "EXPLAIN (FORMAT $$text$$) SELECT 1",
        ] {
            accept(sql)?;
        }
        assert_eq!(
            reject("EXPLAIN (FORMAT $$json$$) SELECT 1")?.0,
            sqlstate::FEATURE_NOT_SUPPORTED
        );
        Ok(())
    }

    #[test]
    fn the_british_spelling_reports_as_analyze() -> anyhow::Result<()> {
        assert!(accept("EXPLAIN (ANALYSE) SELECT 1")?.analyze);
        // PG folds ANALYSE to "analyze" before reading the value, so the message
        // does too.
        assert_eq!(
            reject("EXPLAIN (ANALYSE bogus) SELECT 1")?,
            (
                sqlstate::SYNTAX_ERROR,
                "analyze requires a Boolean value".to_string()
            )
        );
        Ok(())
    }

    #[test]
    fn an_unknown_option_is_a_syntax_error() -> anyhow::Result<()> {
        // PG reports 42601 here, not 22023, and echoes the *folded* name: an
        // unquoted option name lowercases like any identifier.
        assert_eq!(
            reject("EXPLAIN (BoGuS) SELECT 1")?,
            (
                sqlstate::SYNTAX_ERROR,
                "unrecognized EXPLAIN option \"bogus\"".to_string()
            )
        );
        // A quoted name keeps its spelling, and never matches a known option.
        assert_eq!(
            reject("EXPLAIN (\"ANALYZE\") SELECT 1")?,
            (
                sqlstate::SYNTAX_ERROR,
                "unrecognized EXPLAIN option \"ANALYZE\"".to_string()
            )
        );
        Ok(())
    }

    #[test]
    fn a_non_boolean_flag_value_is_rejected() -> anyhow::Result<()> {
        // PG's wording, down to the lowercased option name and the 42601.
        for sql in [
            "EXPLAIN (ANALYZE bogus) SELECT 1",
            "EXPLAIN (analyze 2) SELECT 1",
        ] {
            assert_eq!(
                reject(sql)?,
                (
                    sqlstate::SYNTAX_ERROR,
                    "analyze requires a Boolean value".to_string()
                ),
                "{sql}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_value_taking_option_given_no_value_is_rejected() -> anyhow::Result<()> {
        assert_eq!(
            reject("EXPLAIN (FORMAT) SELECT 1")?,
            (
                sqlstate::SYNTAX_ERROR,
                "format requires a parameter".to_string()
            )
        );
        Ok(())
    }

    #[test]
    fn a_repeated_option_takes_the_last_value() -> anyhow::Result<()> {
        // PG applies the options in order, so the later one wins.
        let opts = accept("EXPLAIN (ANALYZE, ANALYZE OFF) SELECT 1")?;
        assert!(!opts.analyze);
        assert!(!opts.summary);
        // SUMMARY set before ANALYZE still wins over ANALYZE's default.
        let opts = accept("EXPLAIN (SUMMARY OFF, ANALYZE) SELECT 1")?;
        assert!(opts.analyze);
        assert!(!opts.summary);
        Ok(())
    }

    #[test]
    fn switching_an_unsupported_option_back_off_is_accepted() -> anyhow::Result<()> {
        // The rejection must not latch while the list is still being read: PG
        // accepts every one of these because the final value is off.
        for sql in [
            "EXPLAIN (MEMORY ON, MEMORY OFF) SELECT 1",
            "EXPLAIN (VERBOSE ON, VERBOSE OFF) SELECT 1",
            "EXPLAIN (SETTINGS ON, SETTINGS OFF) SELECT 1",
            "EXPLAIN (WAL ON, WAL OFF) SELECT 1",
            "EXPLAIN (TIMING ON, TIMING OFF) SELECT 1",
            "EXPLAIN (GENERIC_PLAN ON, GENERIC_PLAN OFF) SELECT 1",
            "EXPLAIN (SERIALIZE text, SERIALIZE none) SELECT 1",
            "EXPLAIN (FORMAT json, FORMAT text) SELECT 1",
        ] {
            accept(sql)?;
        }
        // ...and switching one back *on* is still rejected.
        assert_eq!(
            reject("EXPLAIN (MEMORY OFF, MEMORY ON) SELECT 1")?.0,
            sqlstate::FEATURE_NOT_SUPPORTED
        );
        Ok(())
    }

    #[test]
    fn the_error_a_bad_option_list_reports_does_not_depend_on_option_order() -> anyhow::Result<()> {
        // PG's checks run once the list is settled, so each pair reports the same
        // error whichever way round it is written.
        for (a, b, expected) in [
            (
                "FORMAT bogus, WAL ON",
                "WAL ON, FORMAT bogus",
                "unrecognized value for EXPLAIN option \"format\": \"bogus\"",
            ),
            (
                "TIMING ON, WAL ON",
                "WAL ON, TIMING ON",
                "EXPLAIN option WAL requires ANALYZE",
            ),
            (
                "SERIALIZE text, TIMING ON",
                "TIMING ON, SERIALIZE text",
                "EXPLAIN option TIMING requires ANALYZE",
            ),
            (
                "ANALYZE, GENERIC_PLAN",
                "GENERIC_PLAN, ANALYZE",
                "EXPLAIN options ANALYZE and GENERIC_PLAN cannot be used together",
            ),
        ] {
            let first = reject(&format!("EXPLAIN ({a}) SELECT 1"))?;
            let second = reject(&format!("EXPLAIN ({b}) SELECT 1"))?;
            assert_eq!(first.1, expected, "EXPLAIN ({a})");
            assert_eq!(first, second, "EXPLAIN ({a}) vs EXPLAIN ({b})");
        }
        Ok(())
    }
}
