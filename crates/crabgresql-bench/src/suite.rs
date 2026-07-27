//! What a benchmark *is*, independent of how it is run.

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
    /// Comma-separated with a `HEADER` line.
    Csv,
}

impl Suite {
    /// The DDL to create the table, optionally pinned to an access method
    /// (`USING parquet`). The suite files carry no `USING` clause, so the
    /// clause is appended after the trailing semicolon is trimmed.
    pub fn schema(&self, access_method: Option<&str>) -> String {
        let ddl = self.schema_sql.trim().trim_end_matches(';');
        match access_method {
            Some(am) => format!("{ddl} USING {am}"),
            None => ddl.to_string(),
        }
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

    /// The `COPY` statement that loads this suite's raw file from stdin.
    pub fn copy_statement(&self) -> String {
        match self.format {
            DataFormat::Tsv => format!("COPY {} FROM STDIN", self.table),
            DataFormat::Csv => format!("COPY {} FROM STDIN WITH (FORMAT csv, HEADER)", self.table),
        }
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

    #[test]
    fn schema_appends_the_access_method_after_the_semicolon() {
        assert_eq!(SUITE.schema(None), "CREATE TABLE hits (id BIGINT)");
        assert_eq!(
            SUITE.schema(Some("parquet")),
            "CREATE TABLE hits (id BIGINT) USING parquet"
        );
    }

    #[test]
    fn queries_are_numbered_from_one_skipping_blanks_and_comments() {
        let queries = SUITE.queries();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].number, 1);
        assert_eq!(queries[1].sql, "SELECT 2;");
    }
}
