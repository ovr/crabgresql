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
        assert!(CLICKBENCH.schema_statements(Some("parquet"))?[0].ends_with("USING parquet;"));
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

    #[test]
    fn suites_are_findable_by_name() {
        assert!(find("clickbench").is_some());
        assert!(find("tpch").is_some());
        assert!(find("nope").is_none());
    }
}
