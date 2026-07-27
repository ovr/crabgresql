//! The suite registry. Add a benchmark by dropping its `.sql` files under
//! `suites/<name>/` and listing a [`Suite`] here.

use crate::suite::{DataFormat, Suite};

/// ClickBench (github.com/ClickHouse/ClickBench): 43 analytical queries over
/// one wide, denormalized web-analytics table (`hits`, 105 columns). The
/// schema and queries are the upstream `postgresql/` variant, unmodified.
pub const CLICKBENCH: Suite = Suite {
    name: "clickbench",
    description: "ClickBench: 43 analytical queries over the 105-column `hits` table",
    table: "hits",
    schema_sql: include_str!("../suites/clickbench/create.sql"),
    queries_sql: include_str!("../suites/clickbench/queries.sql"),
    dataset_url: "https://datasets.clickhouse.com/hits_compatible/hits.tsv.gz",
    format: DataFormat::Tsv,
};

pub const ALL: &[&Suite] = &[&CLICKBENCH];

pub fn find(name: &str) -> Option<&'static Suite> {
    ALL.iter().copied().find(|suite| suite.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clickbench_has_43_queries_and_a_105_column_schema() {
        assert_eq!(CLICKBENCH.queries().len(), 43);
        assert_eq!(CLICKBENCH.schema(None).matches("NOT NULL").count(), 105);
    }

    #[test]
    fn suites_are_findable_by_name() {
        assert!(find("clickbench").is_some());
        assert!(find("nope").is_none());
    }
}
