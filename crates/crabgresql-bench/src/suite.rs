//! What a benchmark *is*, independent of how it is run.

use anyhow::{Result, bail};

/// One benchmark: the DDL, where its data comes from, and the queries to time.
pub struct Suite {
    pub name: &'static str,
    pub description: &'static str,
    /// The table the dataset is loaded into (and dropped from on a reload).
    pub table: &'static str,
    /// `CREATE TABLE` DDL, verbatim from the upstream benchmark.
    pub schema_sql: &'static str,
    /// The queries, one per line, verbatim from the upstream benchmark.
    pub queries_sql: &'static str,
    /// Where the raw data file can be obtained, for the "no data" hint.
    pub dataset_url: &'static str,
    /// Encoding of that raw data file.
    pub format: DataFormat,
}

/// The wire format of a suite's raw data file, which decides the `COPY` we run.
#[derive(Clone, Copy, Debug)]
pub enum DataFormat {
    /// Tab-separated, PostgreSQL `COPY … FROM STDIN` text escaping.
    Tsv,
    /// Comma-separated with a header line, which the loader strips itself.
    Csv,
}

impl Suite {
    /// The DDL to create the table, optionally pinned to an access method
    /// (`USING parquet`).
    ///
    /// The suite files carry no `USING` clause, so it is spliced in before the
    /// terminating semicolon — but only after checking that the file really is
    /// the one bare statement that splicing assumes. Appending blind would
    /// silently do the wrong thing for a file that ends in a `-- comment` (the
    /// clause lands inside the comment and the run measures the default heap
    /// while reporting the requested access method).
    pub fn schema(&self, access_method: Option<&str>) -> Result<String> {
        let ddl = self.schema_sql.trim();
        let Some(am) = access_method else {
            return Ok(ddl.to_string());
        };
        let Some(body) = ddl.strip_suffix(';') else {
            bail!(
                "cannot apply `USING {am}`: {}'s schema does not end in a bare `;` \
                 (a trailing comment would swallow the clause)",
                self.name,
            );
        };
        if body.contains(';') {
            bail!(
                "cannot apply `USING {am}`: {}'s schema holds more than one statement",
                self.name,
            );
        }
        Ok(format!("{body} USING {am}"))
    }

    /// The queries in file order, skipping blanks and `--` comments. The
    /// upstream files put exactly one query on each line, which is what keeps
    /// query numbering stable across systems.
    pub fn queries(&self) -> Vec<Query> {
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

    /// The `COPY` statement that loads one batch of this suite's raw file.
    ///
    /// Note the absence of `HEADER` for CSV: the loader strips the header line
    /// itself, because `HEADER` would drop the first *data* line of every batch
    /// after the first.
    pub fn copy_statement(&self) -> String {
        match self.format {
            DataFormat::Tsv => format!("COPY {} FROM STDIN", self.table),
            DataFormat::Csv => format!("COPY {} FROM STDIN WITH (FORMAT csv)", self.table),
        }
    }

    /// Whether the raw file starts with a header line the loader must skip.
    pub fn has_header(&self) -> bool {
        matches!(self.format, DataFormat::Csv)
    }
}

/// One timed query, numbered from 1 as the upstream results tables number them.
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
        table: "hits",
        schema_sql: "CREATE TABLE hits (id BIGINT);\n",
        queries_sql: "SELECT 1;\n\n-- comment\nSELECT 2;\n",
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
        assert_eq!(SUITE.schema(None)?, "CREATE TABLE hits (id BIGINT);");
        assert_eq!(
            SUITE.schema(Some("parquet"))?,
            "CREATE TABLE hits (id BIGINT) USING parquet"
        );
        Ok(())
    }

    #[test]
    fn schema_refuses_to_splice_where_the_clause_would_be_swallowed() {
        // A trailing line comment would hide `USING parquet` and silently
        // benchmark the default heap instead.
        let commented = with_schema("CREATE TABLE hits (id BIGINT);\n-- upstream note\n");
        assert!(commented.schema(Some("parquet")).is_err());
        // …but with no access method requested the file is used as-is.
        assert!(commented.schema(None).is_ok());

        let two_statements =
            with_schema("CREATE TABLE hits (id BIGINT);\nCREATE INDEX i ON hits (id);\n");
        assert!(two_statements.schema(Some("parquet")).is_err());
    }

    #[test]
    fn queries_are_numbered_from_one_skipping_blanks_and_comments() {
        let queries = SUITE.queries();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].number, 1);
        assert_eq!(queries[1].sql, "SELECT 2;");
    }

    #[test]
    fn csv_copy_omits_header_because_the_loader_strips_it() {
        let csv = Suite {
            format: DataFormat::Csv,
            ..SUITE
        };
        assert!(csv.has_header());
        assert!(!csv.copy_statement().contains("HEADER"));
        assert!(!SUITE.has_header());
    }
}
