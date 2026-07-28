//! What a benchmark *is*, independent of how it is run.

use anyhow::{Result, bail};

/// One benchmark: the DDL, where its data comes from, and the queries to time.
pub struct Suite {
    pub name: &'static str,
    pub description: &'static str,
    /// The tables the dataset is loaded into, in load order, and dropped from
    /// on a reload. A suite with more than one of them reads its data from a
    /// directory rather than a single file — see [`Suite::is_multi_table`].
    pub tables: &'static [&'static str],
    /// `CREATE TABLE` DDL, one statement per table in `tables` and in the same
    /// order.
    pub schema_sql: &'static str,
    /// The queries, transcribed from the upstream benchmark, laid out as
    /// `queries_format` says.
    pub queries_sql: &'static str,
    pub queries_format: QueryFormat,
    /// How to obtain the raw data, printed verbatim when there is none. Worth
    /// being a runnable recipe rather than a bare URL: "here is a link to a
    /// .gz" is not enough to tell someone what to pass to `--data`.
    pub dataset_hint: &'static str,
    /// Encoding of the raw data files.
    pub format: DataFormat,
}

/// How a suite's queries are laid out in its `queries.sql`.
#[derive(Clone, Copy, Debug)]
pub enum QueryFormat {
    /// Exactly one query per line, as ClickBench publishes them.
    OnePerLine,
    /// Multi-line queries, each introduced by a `-- Qn` marker line. The number
    /// comes from the marker rather than from position in the file, so
    /// commenting a query out does not renumber the ones after it.
    ///
    /// `-- Qn setup` and `-- Qn teardown` sections carry statements that run
    /// outside the timed window — TPC-H Q15 needs a view created before it and
    /// dropped after, and neither the DDL's cost nor its cleanup belongs in the
    /// measurement.
    Numbered,
}

/// The wire format of a suite's raw data files, which decides the `COPY` we run.
#[derive(Clone, Copy, Debug)]
pub enum DataFormat {
    /// Tab-separated, PostgreSQL `COPY … FROM STDIN` text escaping.
    Tsv,
    /// Comma-separated with a header line, which the loader strips itself.
    Csv,
    /// Pipe-separated without a header, the shape TPC-H's `.tbl` files use.
    Psv,
}

impl DataFormat {
    /// The file extension a per-table data file is expected to carry. Only a
    /// multi-table suite uses this; a single-table suite is pointed straight at
    /// its file.
    pub fn extension(self) -> &'static str {
        match self {
            DataFormat::Tsv => "tsv",
            DataFormat::Csv => "csv",
            DataFormat::Psv => "tbl",
        }
    }

    /// The delimiter this format allows at the end of a line, which the loader
    /// strips.
    ///
    /// TPC's own `dbgen` terminates every `.tbl` line with `|`, which `COPY`
    /// reads as one more empty column than the table has. Refusing that file
    /// would be refusing the benchmark's canonical generator, so the trailing
    /// delimiter is dropped on the way in.
    pub fn trailing_delimiter(self) -> Option<u8> {
        match self {
            DataFormat::Psv => Some(b'|'),
            DataFormat::Tsv | DataFormat::Csv => None,
        }
    }
}

impl Suite {
    /// Whether this suite's data lives in a directory of per-table files rather
    /// than in one file.
    pub fn is_multi_table(&self) -> bool {
        self.tables.len() > 1
    }

    /// The DDL statements that create the suite's tables, optionally pinned to
    /// an access method (`USING parquet`).
    ///
    /// The file is split on `;`, which is only sound if it holds exactly the
    /// bare `CREATE TABLE`s it claims to — a `;` inside a comment or a string
    /// literal would cut a statement in half. So every fragment is checked to
    /// be the `CREATE TABLE` for the table `tables` names at that position,
    /// which catches the mangled fragment, a schema that has drifted from
    /// `tables`, and anything that is not a plain table definition. The check
    /// runs whether or not an access method was asked for, because a mangled
    /// statement is just as wrong on the default heap.
    pub fn schema_statements(&self, access_method: Option<&str>) -> Result<Vec<String>> {
        let statements: Vec<&str> = self
            .schema_sql
            .split(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
            .collect();
        if statements.len() != self.tables.len() {
            bail!(
                "{} declares {} tables but its schema holds {} statements",
                self.name,
                self.tables.len(),
                statements.len(),
            );
        }

        let mut out = Vec::with_capacity(statements.len());
        for (statement, table) in statements.iter().zip(self.tables) {
            // A leading comment is fine — it cannot swallow a clause appended
            // at the end — so look past it before matching.
            let body = strip_leading_comments(statement);
            let expected = format!("create table {table} ");
            if !body
                .to_ascii_lowercase()
                .starts_with(expected.trim_end_matches(' '))
            {
                bail!(
                    "{}'s schema does not define `{table}` where it says it does; \
                     got `{}`",
                    self.name,
                    body.lines().next().unwrap_or(body).trim(),
                );
            }
            match access_method {
                None => out.push(format!("{statement};")),
                Some(am) => {
                    // Splicing appends after the last line, so a comment there
                    // would swallow the clause and the run would measure the
                    // default heap while reporting the requested storage.
                    if statement
                        .lines()
                        .next_back()
                        .is_some_and(|last| last.contains("--") || last.trim_end().ends_with("*/"))
                    {
                        bail!(
                            "cannot apply `USING {am}`: {}'s `{table}` statement ends \
                             in a comment, which would swallow the clause",
                            self.name,
                        );
                    }
                    out.push(format!("{statement} USING {am};"));
                }
            }
        }
        Ok(out)
    }

    /// The queries in file order, skipping blanks and comments.
    pub fn queries(&self) -> Vec<Query> {
        match self.queries_format {
            QueryFormat::OnePerLine => self.one_per_line_queries(),
            QueryFormat::Numbered => self.numbered_queries(),
        }
    }

    /// One query per line, numbered by position — which is what keeps query
    /// numbering stable against the published results.
    fn one_per_line_queries(&self) -> Vec<Query> {
        self.queries_sql
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("--"))
            .enumerate()
            .map(|(i, sql)| Query {
                number: i + 1,
                sql: sql.to_string(),
                setup: String::new(),
                teardown: String::new(),
            })
            .collect()
    }

    /// Multi-line queries delimited by `-- Qn` markers, numbered by the marker.
    /// Anything before the first marker is preamble and is dropped.
    ///
    /// A `-- Qn setup` or `-- Qn teardown` section attaches to the query of the
    /// same number instead of becoming one of its own.
    fn numbered_queries(&self) -> Vec<Query> {
        let mut queries: Vec<Query> = Vec::new();
        let mut part = QueryPart::Body;
        for line in self.queries_sql.lines() {
            if let Some((number, marked)) = query_marker(line) {
                part = marked;
                if !queries.iter().any(|q: &Query| q.number == number) {
                    queries.push(Query {
                        number,
                        sql: String::new(),
                        setup: String::new(),
                        teardown: String::new(),
                    });
                }
                continue;
            }
            // Sections of one query may be written in any order, so find it by
            // number rather than assuming it is the one most recently pushed.
            let Some(current) = queries.last_mut() else {
                continue;
            };
            let target = match part {
                QueryPart::Body => &mut current.sql,
                QueryPart::Setup => &mut current.setup,
                QueryPart::Teardown => &mut current.teardown,
            };
            target.push_str(line);
            target.push('\n');
        }
        for query in &mut queries {
            query.sql = query.sql.trim().to_string();
            query.setup = query.setup.trim().to_string();
            query.teardown = query.teardown.trim().to_string();
        }
        // A body that is only comments is a query that was commented out, not a
        // query that happens to be short: sending it would come back as an
        // empty-query response and be scored as a pass.
        queries.retain(|query| !is_all_comments(&query.sql));
        queries
    }

    /// The `COPY` statement that loads one batch of `table`'s raw file.
    ///
    /// Note the absence of `HEADER` for CSV: the loader strips the header line
    /// itself, because `HEADER` would drop the first *data* line of every batch
    /// after the first.
    pub fn copy_statement(&self, table: &str) -> String {
        match self.format {
            DataFormat::Tsv => format!("COPY {table} FROM STDIN"),
            DataFormat::Csv => format!("COPY {table} FROM STDIN WITH (FORMAT csv)"),
            DataFormat::Psv => {
                format!("COPY {table} FROM STDIN WITH (FORMAT csv, DELIMITER '|')")
            }
        }
    }

    /// Whether the raw files start with a header line the loader must skip.
    pub fn has_header(&self) -> bool {
        matches!(self.format, DataFormat::Csv)
    }
}

/// Which section of a query a `-- Qn …` marker opens.
#[derive(Clone, Copy, Debug, PartialEq)]
enum QueryPart {
    Body,
    Setup,
    Teardown,
}

/// The query number and section a `-- Qn` marker line carries, if the line is
/// one. `-- Q15 setup` opens Q15's setup; a bare `-- Q15` opens its body.
fn query_marker(line: &str) -> Option<(usize, QueryPart)> {
    let rest = line.trim().strip_prefix("--")?.trim();
    let digits = rest.strip_prefix(['Q', 'q'])?;
    let split = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let (number, suffix) = digits.split_at(split);
    let part = match suffix.trim() {
        "" => QueryPart::Body,
        "setup" => QueryPart::Setup,
        "teardown" => QueryPart::Teardown,
        // Not a marker at all — a comment that merely starts with a Q.
        _ => return None,
    };
    Some((number.parse().ok()?, part))
}

/// Whether a block of SQL holds nothing the server would execute.
fn is_all_comments(sql: &str) -> bool {
    sql.lines()
        .map(str::trim)
        .all(|line| line.is_empty() || line.starts_with("--"))
}

/// A statement's text with any leading comment lines removed.
fn strip_leading_comments(statement: &str) -> &str {
    let mut rest = statement.trim_start();
    while rest.starts_with("--") {
        rest = match rest.find('\n') {
            Some(end) => rest[end + 1..].trim_start(),
            None => "",
        };
    }
    rest
}

/// One timed query, numbered from 1 as the upstream results tables number it.
///
/// `setup` and `teardown` run outside the timed window. `teardown` runs even
/// when the query failed — TPC-H Q15 creates a view, and a leaked view turns
/// every later run of that query into a bogus "already exists" failure.
#[derive(Clone, Debug)]
pub struct Query {
    pub number: usize,
    pub sql: String,
    pub setup: String,
    pub teardown: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUITE: Suite = Suite {
        name: "t",
        description: "",
        tables: &["hits"],
        schema_sql: "CREATE TABLE hits (id BIGINT);\n",
        queries_sql: "SELECT 1;\n\n-- comment\nSELECT 2;\n",
        queries_format: QueryFormat::OnePerLine,
        dataset_hint: "",
        format: DataFormat::Tsv,
    };

    fn with_schema(schema_sql: &'static str) -> Suite {
        Suite {
            schema_sql,
            ..SUITE
        }
    }

    fn numbered(queries_sql: &'static str) -> Suite {
        Suite {
            queries_sql,
            queries_format: QueryFormat::Numbered,
            ..SUITE
        }
    }

    #[test]
    fn schema_splices_the_access_method_before_the_semicolon() -> Result<()> {
        assert_eq!(
            SUITE.schema_statements(None)?,
            ["CREATE TABLE hits (id BIGINT);"]
        );
        assert_eq!(
            SUITE.schema_statements(Some("parquet"))?,
            ["CREATE TABLE hits (id BIGINT) USING parquet;"]
        );
        Ok(())
    }

    #[test]
    fn schema_splices_every_statement_of_a_multi_table_suite() -> Result<()> {
        let suite = Suite {
            tables: &["a", "b"],
            schema_sql: "CREATE TABLE a (id INT);\nCREATE TABLE b (id INT);\n",
            ..SUITE
        };
        let statements = suite.schema_statements(Some("parquet"))?;
        assert_eq!(statements.len(), 2);
        assert!(statements.iter().all(|s| s.ends_with("USING parquet;")));
        Ok(())
    }

    #[test]
    fn schema_refuses_to_splice_where_the_clause_would_be_swallowed() {
        // A comment at the end of the statement would hide `USING parquet` and
        // silently benchmark the default heap instead.
        let trailing = with_schema("CREATE TABLE hits (id BIGINT) -- note\n;");
        assert!(trailing.schema_statements(Some("parquet")).is_err());
        // …but with no access method requested there is no clause to swallow.
        assert!(trailing.schema_statements(None).is_ok());
    }

    #[test]
    fn a_leading_comment_does_not_block_splicing() -> Result<()> {
        // It cannot swallow a clause appended after the last line, so refusing
        // it would reject an upstream file that merely carries an attribution
        // header.
        let headed = with_schema("-- upstream header\nCREATE TABLE hits (id BIGINT);\n");
        assert!(headed.schema_statements(Some("parquet"))?[0].ends_with("USING parquet;"));
        Ok(())
    }

    #[test]
    fn schema_must_define_the_tables_the_suite_declares() {
        // Drift between `tables` and the DDL: caught with or without an access
        // method, because a stray table is created and never loaded either way.
        let extra =
            with_schema("CREATE TABLE hits (id BIGINT);\nCREATE TABLE other (id BIGINT);\n");
        assert!(extra.schema_statements(Some("parquet")).is_err());
        assert!(extra.schema_statements(None).is_err());

        // Wrong table in the right slot.
        let renamed = with_schema("CREATE TABLE clicks (id BIGINT);\n");
        assert!(renamed.schema_statements(None).is_err());

        // A `;` inside a comment cuts the statement in two; the tail is not a
        // CREATE TABLE, so the mangling is caught rather than executed.
        let mangled = with_schema("CREATE TABLE hits (\n  id BIGINT -- see spec; note\n);\n");
        assert!(mangled.schema_statements(None).is_err());
    }

    #[test]
    fn queries_are_numbered_from_one_skipping_blanks_and_comments() {
        let queries = SUITE.queries();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].number, 1);
        assert_eq!(queries[1].sql, "SELECT 2;");
    }

    #[test]
    fn numbered_queries_take_their_number_from_the_marker() {
        let suite = numbered("preamble\n-- Q1\nselect 1\nfrom t;\n-- Q7\nselect 7;\n-- Q9\n");
        let queries = suite.queries();
        // Q9 has no body, so it is not a query; the preamble is dropped.
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].number, 1);
        assert_eq!(queries[0].sql, "select 1\nfrom t;");
        assert_eq!(queries[1].number, 7);
    }

    #[test]
    fn a_commented_out_query_is_dropped_not_sent_as_an_empty_one() {
        // The server answers an all-comment string with an empty-query
        // response, which the runner would otherwise score as a passing run.
        let suite = numbered("-- Q1\n-- select 1;\n-- Q2\nselect 2;\n");
        let queries = suite.queries();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].number, 2);
    }

    #[test]
    fn setup_and_teardown_attach_to_the_query_of_the_same_number() {
        let suite = numbered(
            "-- Q1 setup\ncreate view v as select 1;\n\
             -- Q1\nselect * from v;\n\
             -- Q1 teardown\ndrop view v;\n\
             -- Q2\nselect 2;\n",
        );
        let queries = suite.queries();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].number, 1);
        assert_eq!(queries[0].setup, "create view v as select 1;");
        assert_eq!(queries[0].sql, "select * from v;");
        assert_eq!(queries[0].teardown, "drop view v;");
        assert!(queries[1].setup.is_empty());
    }

    #[test]
    fn a_comment_that_merely_starts_with_q_is_not_a_marker() {
        let suite = numbered("-- Q1\nselect 1;\n-- Query tuning note\nselect 2;\n");
        let queries = suite.queries();
        // The note and the line after it belong to Q1, not to a new query.
        assert_eq!(queries.len(), 1);
        assert!(queries[0].sql.contains("select 2;"));
    }

    #[test]
    fn csv_copy_omits_header_because_the_loader_strips_it() {
        let csv = Suite {
            format: DataFormat::Csv,
            ..SUITE
        };
        assert!(csv.has_header());
        assert!(!csv.copy_statement("hits").contains("HEADER"));
        assert!(!SUITE.has_header());
    }

    #[test]
    fn pipe_separated_data_copies_with_an_explicit_delimiter() {
        let psv = Suite {
            format: DataFormat::Psv,
            ..SUITE
        };
        assert!(!psv.has_header());
        assert_eq!(psv.format.extension(), "tbl");
        assert_eq!(
            psv.copy_statement("lineitem"),
            "COPY lineitem FROM STDIN WITH (FORMAT csv, DELIMITER '|')"
        );
        // dbgen ends every line with the delimiter; the loader drops it.
        assert_eq!(psv.format.trailing_delimiter(), Some(b'|'));
        assert_eq!(DataFormat::Tsv.trailing_delimiter(), None);
    }
}
