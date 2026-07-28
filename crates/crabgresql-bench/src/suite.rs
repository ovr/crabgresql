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
    /// `CREATE TABLE` DDL, verbatim from the upstream benchmark, one statement
    /// per table in `tables`.
    pub schema_sql: &'static str,
    /// The queries, verbatim from the upstream benchmark, laid out as
    /// `queries_format` says.
    pub queries_sql: &'static str,
    pub queries_format: QueryFormat,
    /// Where the raw data can be obtained, for the "no data" hint.
    pub dataset_url: &'static str,
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
    /// The suite files carry no `USING` clause, so it is spliced in before each
    /// terminating semicolon — but only after checking that the file really is
    /// the bare statements that splicing assumes. Appending blind would
    /// silently do the wrong thing for a file carrying a comment (the clause
    /// lands inside the comment and the run measures the default heap while
    /// reporting the requested access method).
    ///
    /// Splitting on `;` assumes no statement holds one inside a string literal,
    /// which is true of DDL that is only `CREATE TABLE`s.
    pub fn schema_statements(&self, access_method: Option<&str>) -> Result<Vec<String>> {
        let statements: Vec<&str> = self
            .schema_sql
            .split(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
            .collect();
        let Some(am) = access_method else {
            return Ok(statements.iter().map(|s| format!("{s};")).collect());
        };
        for statement in &statements {
            if statement.contains("--") || statement.contains("/*") {
                bail!(
                    "cannot apply `USING {am}`: {}'s schema carries a comment, \
                     which would swallow the clause",
                    self.name,
                );
            }
        }
        if statements.len() != self.tables.len() {
            bail!(
                "cannot apply `USING {am}`: {} declares {} tables but its schema \
                 holds {} statements",
                self.name,
                self.tables.len(),
                statements.len(),
            );
        }
        Ok(statements
            .iter()
            .map(|statement| format!("{statement} USING {am};"))
            .collect())
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
            })
            .collect()
    }

    /// Multi-line queries delimited by `-- Qn` markers, numbered by the marker.
    /// Anything before the first marker is preamble and is dropped.
    fn numbered_queries(&self) -> Vec<Query> {
        let mut queries: Vec<Query> = Vec::new();
        for line in self.queries_sql.lines() {
            if let Some(number) = query_marker(line) {
                queries.push(Query {
                    number,
                    sql: String::new(),
                });
                continue;
            }
            let Some(current) = queries.last_mut() else {
                continue;
            };
            current.sql.push_str(line);
            current.sql.push('\n');
        }
        for query in &mut queries {
            query.sql = query.sql.trim().to_string();
        }
        queries.retain(|query| !query.sql.is_empty());
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

/// The query number a `-- Qn` marker line carries, if the line is one.
fn query_marker(line: &str) -> Option<usize> {
    let rest = line.trim().strip_prefix("--")?.trim();
    let digits = rest.strip_prefix('Q').or_else(|| rest.strip_prefix('q'))?;
    digits.parse().ok()
}

/// One timed query, numbered from 1 as the upstream results tables number it.
#[derive(Clone, Debug)]
pub struct Query {
    pub number: usize,
    pub sql: String,
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
        dataset_url: "",
        format: DataFormat::Tsv,
    };

    fn with_schema(schema_sql: &'static str) -> Suite {
        Suite {
            schema_sql,
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
        // A line comment would hide `USING parquet` and silently benchmark the
        // default heap instead.
        let commented = with_schema("CREATE TABLE hits (id BIGINT);\n-- upstream note\n");
        assert!(commented.schema_statements(Some("parquet")).is_err());
        // …but with no access method requested the file is used as-is.
        assert!(commented.schema_statements(None).is_ok());

        // A statement the suite never declared a table for means the two have
        // drifted apart, and splicing would create something unnamed.
        let extra =
            with_schema("CREATE TABLE hits (id BIGINT);\nCREATE TABLE other (id BIGINT);\n");
        assert!(extra.schema_statements(Some("parquet")).is_err());
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
        let suite = Suite {
            queries_sql: "preamble\n-- Q1\nselect 1\nfrom t;\n-- Q7\nselect 7;\n-- Q9\n",
            queries_format: QueryFormat::Numbered,
            ..SUITE
        };
        let queries = suite.queries();
        // Q9 has no body, so it is not a query; the preamble is dropped.
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].number, 1);
        assert_eq!(queries[0].sql, "select 1\nfrom t;");
        assert_eq!(queries[1].number, 7);
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
    }
}
