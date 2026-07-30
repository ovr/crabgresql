//! The suite registry. Add a benchmark by dropping its `.sql` files under
//! `suites/<name>/` and listing a [`Suite`] here.

use crate::suite::{DataFormat, QueryFormat, Suite};

/// ClickBench (github.com/ClickHouse/ClickBench): 43 analytical queries over
/// one wide, denormalized web-analytics table (`hits`, 105 columns). The
/// schema and queries are the upstream `postgresql/` variant, unmodified.
pub const CLICKBENCH: Suite = Suite {
    name: "clickbench",
    description: "ClickBench: 43 analytical queries over the 105-column `hits` table",
    tables: &["hits"],
    // ClickBench's own ClickHouse variant declares exactly this `ORDER BY`; the
    // `postgresql/` variant has no key to copy because PostgreSQL has nowhere to
    // put one.
    sort_keys: &["CounterID, EventDate, UserID, EventTime, WatchID"],
    schema_sql: include_str!("../suites/clickbench/create.sql"),
    queries_sql: include_str!("../suites/clickbench/queries.sql"),
    queries_format: QueryFormat::OnePerLine,
    dataset_hint: "  curl -sS https://datasets.clickhouse.com/hits_compatible/hits.tsv.gz \\\n\
                   \x20   | gzip -dc | head -n 1000000 > hits.tsv\n\
                   then re-run with --data hits.tsv (uncompressed — the loader \
                   does not decompress).",
    format: DataFormat::Tsv,
};

/// TPC-H: 22 join-heavy decision-support queries over an 8-table order
/// schema, the complement to ClickBench's single wide table.
///
/// The queries are the specification's own text — implicit comma joins, no
/// hand-rewriting into `JOIN … ON`. Rewriting them would measure the rewrite
/// rather than the planner, and a query the planner handles badly is exactly
/// what this harness exists to surface.
pub const TPCH: Suite = Suite {
    name: "tpch",
    description: "TPC-H: 22 decision-support queries over 8 tables",
    // Load order: referenced tables first, so the DDL could grow foreign keys.
    tables: &[
        "region", "nation", "part", "supplier", "partsupp", "customer", "orders", "lineitem",
    ],
    // The specification's own primary keys, in `tables` order. The DDL does not
    // declare them — TPC-H's schema clause leaves keys to the implementation —
    // so they are named here instead.
    sort_keys: &[
        "r_regionkey",
        "n_nationkey",
        "p_partkey",
        "s_suppkey",
        "ps_partkey, ps_suppkey",
        "c_custkey",
        "o_orderkey",
        "l_orderkey, l_linenumber",
    ],
    schema_sql: include_str!("../suites/tpch/create.sql"),
    queries_sql: include_str!("../suites/tpch/queries.sql"),
    queries_format: QueryFormat::Numbered,
    // No public download: TPC-H data is generated.
    dataset_hint: "  duckdb tpch.duckdb -c \"INSTALL tpch; LOAD tpch; CALL dbgen(sf=0.01);\"\n\
                   \x20 mkdir -p tpch\n\
                   \x20 for t in region nation part supplier partsupp customer orders lineitem; do\n\
                   \x20     duckdb tpch.duckdb -c \\\n\
                   \x20       \"COPY $t TO 'tpch/$t.tbl' (FORMAT csv, DELIMITER '|', HEADER false);\"\n\
                   \x20 done\n\
                   then re-run with --data tpch/ (the directory, not a file).",
    format: DataFormat::Psv,
};

pub const ALL: &[&Suite] = &[&CLICKBENCH, &TPCH];

pub fn find(name: &str) -> Option<&'static Suite> {
    ALL.iter().copied().find(|suite| suite.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clickbench_has_43_queries_and_a_105_column_schema() -> anyhow::Result<()> {
        assert_eq!(CLICKBENCH.queries().len(), 43);
        assert_eq!(CLICKBENCH.schema_statements(None)?.len(), 1);
        assert_eq!(
            CLICKBENCH.schema_statements(None)?[0]
                .matches("NOT NULL")
                .count(),
            105
        );
        // The vendored file must stay spliceable, or `--using` cannot work.
        assert!(CLICKBENCH.schema_statements(Some("parquet"))?[0].ends_with(
            "USING parquet ORDER BY (CounterID, EventDate, UserID, EventTime, WatchID);"
        ));
        Ok(())
    }

    #[test]
    fn tpch_has_22_queries_numbered_one_to_twenty_two() {
        let numbers: Vec<usize> = TPCH.queries().iter().map(|q| q.number).collect();
        assert_eq!(numbers, (1..=22).collect::<Vec<_>>());
    }

    #[test]
    fn tpch_declares_one_ddl_statement_per_table() -> anyhow::Result<()> {
        assert_eq!(TPCH.tables.len(), 8);
        assert_eq!(TPCH.schema_statements(None)?.len(), TPCH.tables.len());
        // Every declared table must actually be created, in load order.
        for (table, statement) in TPCH.tables.iter().zip(TPCH.schema_statements(None)?) {
            assert!(
                statement.starts_with(&format!("CREATE TABLE {table} (")),
                "{table}: {statement}"
            );
        }
        // Spliceable, which the old single-statement guard would have refused.
        assert!(TPCH.schema_statements(Some("parquet"))?.len() == 8);
        Ok(())
    }

    /// The declared columns of one `CREATE TABLE <t> (...)`, lowercased.
    ///
    /// Reads the parenthesized body and takes the first token of each
    /// comma-separated item, which is the column name in the plain DDL both
    /// suites use. Deliberately not a substring match on the raw text: pinning
    /// the vendored files' indentation would make a re-vendor that reflows them
    /// fail a test about sort keys.
    fn declared_columns(statement: &str) -> Vec<String> {
        let body = match (statement.find('('), statement.rfind(')')) {
            (Some(open), Some(close)) if open < close => &statement[open + 1..close],
            _ => return Vec::new(),
        };
        body.split(',')
            .filter_map(|item| item.split_whitespace().next())
            .map(str::to_ascii_lowercase)
            .collect()
    }

    /// The sort keys live in this file while the columns they name live in the
    /// vendored `.sql`, so nothing but this check couples them. A typo would
    /// otherwise surface only as a `42703` in a `--using parquet` run, which
    /// needs a dataset nobody has in CI.
    ///
    /// It also enforces what the server enforces: a key names at least one
    /// column and never repeats one (`42P17`).
    #[test]
    fn every_sort_key_names_distinct_columns_of_its_table() -> anyhow::Result<()> {
        for suite in ALL {
            let statements = suite.schema_statements(None)?;
            for ((statement, table), key) in
                statements.iter().zip(suite.tables).zip(suite.sort_keys)
            {
                let declared = declared_columns(statement);
                assert!(
                    !declared.is_empty(),
                    "{}: could not read `{table}`'s column list",
                    suite.name,
                );
                let mut seen: Vec<String> = Vec::new();
                for column in key.split(',').map(str::trim) {
                    let column = column.to_ascii_lowercase();
                    assert!(
                        declared.contains(&column),
                        "{}: `{table}` has no column `{column}`",
                        suite.name,
                    );
                    assert!(
                        !seen.contains(&column),
                        "{}: `{table}`'s sort key repeats `{column}`",
                        suite.name,
                    );
                    seen.push(column);
                }
                assert!(
                    !seen.is_empty(),
                    "{}: `{table}` declares an empty sort key",
                    suite.name,
                );
            }
        }
        Ok(())
    }

    #[test]
    fn suites_are_findable_by_name() {
        assert!(find("clickbench").is_some());
        assert!(find("tpch").is_some());
        assert!(find("nope").is_none());
    }
}
