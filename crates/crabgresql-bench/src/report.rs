//! Results of a run, and the two ways to print them: an aligned table for a
//! human, and JSON shaped like the upstream benchmarks' result files
//! (one array of per-run seconds per query, `null` where the query failed).

use std::fmt::Write as _;
use std::time::Duration;

pub struct SuiteRun {
    pub suite: String,
    /// What was benchmarked, for the report header.
    pub target: String,
    /// The access method the tables were created with, `None` when this run did
    /// not create them and so cannot know. Recorded because a timing is
    /// meaningless without the storage it was measured on.
    pub access_method: Option<String>,
    /// The benchmarked tables and their rows, counted (not assumed) before the
    /// run.
    pub tables: Vec<TableRows>,
    /// Time the load took, when this run loaded the dataset.
    pub load_time: Option<Duration>,
    pub queries: Vec<QueryRun>,
}

/// One loaded table and how many rows it holds.
#[derive(Clone)]
pub struct TableRows {
    pub name: String,
    pub rows: u64,
}

pub struct QueryRun {
    pub number: usize,
    pub runs: Vec<Outcome>,
}

pub enum Outcome {
    Ok {
        elapsed: Duration,
        rows: usize,
    },
    Failed(String),
    /// The run never completed and the connection was abandoned.
    TimedOut,
    /// The connection was lost, so this query was never really measured —
    /// distinct from `Failed`, which is the server rejecting the query.
    Disconnected(String),
}

impl Outcome {
    fn seconds(&self) -> Option<f64> {
        match self {
            Outcome::Ok { elapsed, .. } => Some(elapsed.as_secs_f64()),
            _ => None,
        }
    }

    fn rows(&self) -> Option<usize> {
        match self {
            Outcome::Ok { rows, .. } => Some(*rows),
            _ => None,
        }
    }
}

impl QueryRun {
    /// Fastest successful run, the number the published results compare.
    pub fn best(&self) -> Option<f64> {
        self.runs
            .iter()
            .filter_map(Outcome::seconds)
            .min_by(f64::total_cmp)
    }

    /// Rows the query returned, from its first successful run.
    pub fn rows(&self) -> Option<usize> {
        self.runs.iter().find_map(Outcome::rows)
    }

    /// True if repeated runs of the same query disagreed on how many rows they
    /// returned — the cheapest available signal that a result is not to be
    /// trusted, since a query is otherwise scored purely on not erroring.
    pub fn rows_unstable(&self) -> bool {
        let mut counts = self.runs.iter().filter_map(Outcome::rows);
        let Some(first) = counts.next() else {
            return false;
        };
        counts.any(|count| count != first)
    }

    /// The first thing that went wrong, if anything did.
    pub fn failure(&self) -> Option<&str> {
        self.runs.iter().find_map(|outcome| match outcome {
            Outcome::Ok { .. } => None,
            Outcome::Failed(message) | Outcome::Disconnected(message) => Some(message.as_str()),
            Outcome::TimedOut => Some("timed out"),
        })
    }

    /// One-line verdict, shared by the progress log and the results table so
    /// the two cannot disagree.
    pub fn status(&self) -> String {
        match (self.best(), self.failure()) {
            (Some(_), None) if self.rows_unstable() => {
                "ok, BUT row count differs between runs".to_string()
            }
            (Some(_), None) => "ok".to_string(),
            (_, Some(failure)) => one_line(failure),
            (None, None) => "not run".to_string(),
        }
    }
}

impl SuiteRun {
    pub fn succeeded(&self) -> usize {
        self.queries.iter().filter(|q| q.best().is_some()).count()
    }

    /// Rows across every table under test.
    pub fn total_rows(&self) -> u64 {
        self.tables.iter().map(|table| table.rows).sum()
    }

    /// How the dataset under test is described wherever a number is reported.
    /// A reused dataset's storage is reported as `unknown`, not guessed at.
    fn table_description(&self) -> String {
        let storage = self.access_method.as_deref().unwrap_or("unknown");
        let rows = self.total_rows();
        match self.tables.len() {
            0 | 1 => format!("{rows} rows, {storage} storage"),
            n => format!("{rows} rows across {n} tables, {storage} storage"),
        }
    }

    pub fn table(&self) -> String {
        let runs = self.queries.first().map_or(0, |q| q.runs.len());
        let mut out = String::new();
        let _ = writeln!(out, "\n{} on {}", self.suite, self.target);
        let _ = write!(out, "{}", self.table_description());
        match self.load_time {
            Some(time) => {
                let _ = writeln!(
                    out,
                    ", loaded in {:.1}s ({:.0} rows/s)",
                    time.as_secs_f64(),
                    self.total_rows() as f64 / time.as_secs_f64().max(f64::EPSILON),
                );
            }
            None => {
                let _ = writeln!(out, " (loaded by an earlier run)");
            }
        }

        let _ = write!(out, "\n{:>3}", "#");
        for i in 1..=runs {
            let _ = write!(out, " {:>9}", format!("run {i}"));
        }
        let _ = writeln!(out, " {:>9} {:>9}  status", "best", "rows");

        for query in &self.queries {
            let _ = write!(out, "{:>3}", query.number);
            for outcome in &query.runs {
                let _ = match outcome.seconds() {
                    Some(secs) => write!(out, " {secs:>9.3}"),
                    None => write!(out, " {:>9}", "-"),
                };
            }
            let _ = match query.best() {
                Some(best) => write!(out, " {best:>9.3}"),
                None => write!(out, " {:>9}", "-"),
            };
            let _ = match query.rows() {
                Some(rows) => write!(out, " {rows:>9}"),
                None => write!(out, " {:>9}", "-"),
            };
            let _ = writeln!(out, "  {}", query.status());
        }

        let total: f64 = self.queries.iter().filter_map(QueryRun::best).sum();
        let _ = writeln!(
            out,
            "\n{} of {} queries succeeded, {total:.3}s total (best runs)",
            self.succeeded(),
            self.queries.len(),
        );
        out
    }

    /// `{"system": …, "result": [[…], …]}` — the shape ClickBench's own
    /// `results/*.json` files use, so a run can be pasted straight in. The
    /// per-query failure text is kept alongside it, so the artifact carries
    /// the gap list rather than an undifferentiated row of `null`s.
    pub fn json(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{{");
        let _ = writeln!(out, "  \"system\": \"CrabgreSQL\",");
        let _ = writeln!(out, "  \"suite\": \"{}\",", escape(&self.suite));
        let _ = writeln!(out, "  \"target\": \"{}\",", escape(&self.target));
        let _ = writeln!(
            out,
            "  \"access_method\": \"{}\",",
            escape(self.access_method.as_deref().unwrap_or("unknown")),
        );
        let _ = writeln!(out, "  \"rows\": {},", self.total_rows());
        let _ = writeln!(out, "  \"tables\": {{");
        for (i, table) in self.tables.iter().enumerate() {
            let comma = if i + 1 == self.tables.len() { "" } else { "," };
            let _ = writeln!(
                out,
                "    \"{}\": {}{comma}",
                escape(&table.name),
                table.rows
            );
        }
        let _ = writeln!(out, "  }},");
        if let Some(time) = self.load_time {
            let _ = writeln!(out, "  \"load_time\": {:.3},", time.as_secs_f64());
        }
        let _ = writeln!(out, "  \"failures\": {{");
        let failures: Vec<&QueryRun> = self
            .queries
            .iter()
            .filter(|q| q.failure().is_some())
            .collect();
        for (i, query) in failures.iter().enumerate() {
            let comma = if i + 1 == failures.len() { "" } else { "," };
            let _ = writeln!(
                out,
                "    \"{}\": \"{}\"{comma}",
                query.number,
                escape(&one_line(query.failure().unwrap_or("failed"))),
            );
        }
        let _ = writeln!(out, "  }},");
        let _ = writeln!(out, "  \"result\": [");
        for (i, query) in self.queries.iter().enumerate() {
            let runs: Vec<String> = query
                .runs
                .iter()
                .map(|outcome| match outcome.seconds() {
                    Some(secs) => format!("{secs:.3}"),
                    None => "null".to_string(),
                })
                .collect();
            let comma = if i + 1 == self.queries.len() { "" } else { "," };
            let _ = writeln!(out, "    [{}]{comma}", runs.join(", "));
        }
        let _ = writeln!(out, "  ]");
        let _ = writeln!(out, "}}");
        out
    }
}

/// Server errors are multi-line; a results table has room for the first line.
fn one_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or(message).trim();
    if line.chars().count() > 100 {
        format!("{}…", line.chars().take(99).collect::<String>())
    } else {
        line.to_string()
    }
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r', '\t'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(millis: u64, rows: usize) -> Outcome {
        Outcome::Ok {
            elapsed: Duration::from_millis(millis),
            rows,
        }
    }

    fn run() -> SuiteRun {
        SuiteRun {
            suite: "clickbench".to_string(),
            target: "in-process".to_string(),
            access_method: Some("parquet".to_string()),
            tables: vec![TableRows {
                name: "hits".to_string(),
                rows: 10,
            }],
            load_time: Some(Duration::from_secs(1)),
            queries: vec![
                QueryRun {
                    number: 1,
                    runs: vec![ok(500, 1), ok(250, 1)],
                },
                QueryRun {
                    number: 2,
                    runs: vec![Outcome::Failed(
                        "ERROR: no function nope\nline 2".to_string(),
                    )],
                },
            ],
        }
    }

    #[test]
    fn best_is_the_fastest_successful_run() {
        let run = run();
        assert_eq!(run.queries[0].best(), Some(0.25));
        assert_eq!(run.queries[1].best(), None);
    }

    #[test]
    fn table_reports_the_failure_on_one_line() {
        let table = run().table();
        assert!(table.contains("ERROR: no function nope"), "{table}");
        assert!(!table.contains("line 2"), "{table}");
        assert!(table.contains("1 of 2 queries succeeded"), "{table}");
    }

    #[test]
    fn table_names_the_storage_and_row_count_it_measured() {
        let table = run().table();
        assert!(table.contains("10 rows, parquet storage"), "{table}");
    }

    #[test]
    fn a_multi_table_dataset_reports_the_total_and_the_table_count() {
        let mut multi = run();
        multi.tables.push(TableRows {
            name: "orders".to_string(),
            rows: 32,
        });
        let table = multi.table();
        assert!(
            table.contains("42 rows across 2 tables, parquet storage"),
            "{table}"
        );
        let json = multi.json();
        assert!(json.contains("\"rows\": 42"), "{json}");
        assert!(json.contains("\"orders\": 32"), "{json}");
    }

    #[test]
    fn a_row_count_that_moves_between_runs_is_called_out() {
        let stable = QueryRun {
            number: 1,
            runs: vec![ok(1, 7), ok(1, 7)],
        };
        assert!(!stable.rows_unstable());
        assert_eq!(stable.status(), "ok");

        let moving = QueryRun {
            number: 1,
            runs: vec![ok(1, 7), ok(1, 6)],
        };
        assert!(moving.rows_unstable());
        assert!(moving.status().contains("row count differs"));
    }

    #[test]
    fn json_uses_null_for_failed_runs_but_keeps_the_reason() {
        let json = run().json();
        assert!(json.contains("[0.500, 0.250],"), "{json}");
        assert!(json.contains("[null]"), "{json}");
        assert!(json.contains("\"access_method\": \"parquet\""), "{json}");
        assert!(json.contains("\"rows\": 10"), "{json}");
        assert!(
            json.contains("\"2\": \"ERROR: no function nope\""),
            "{json}"
        );
    }
}
