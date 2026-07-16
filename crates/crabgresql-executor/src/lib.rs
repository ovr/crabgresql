//! Volcano (iterator) executor.
//!
//! M0 nodes: `Values` (FROM-less SELECT) and `SeqScan` (full scan through the
//! storage API). Joins, aggregation, sorting and expression evaluation over
//! rows arrive with M1.

use std::sync::Arc;

use crabgresql_storage_api::{TableAm, Tuple};
use crabgresql_types::PgType;

/// One column of a node's result set, as needed for `RowDescription`.
#[derive(Clone, Debug)]
pub struct OutputColumn {
    pub name: String,
    pub ty: PgType,
}

/// A Volcano execution node: `next()` pulls one tuple at a time.
pub trait ExecNode: Send {
    fn next(&mut self) -> Option<Tuple>;
}

/// Materialized constant rows: `SELECT 1`, `VALUES (...)`.
pub struct Values {
    rows: std::vec::IntoIter<Tuple>,
}

impl Values {
    pub fn new(rows: Vec<Tuple>) -> Self {
        Self {
            rows: rows.into_iter(),
        }
    }
}

impl ExecNode for Values {
    fn next(&mut self) -> Option<Tuple> {
        self.rows.next()
    }
}

/// Full table scan through the storage API.
pub struct SeqScan {
    iter: Box<dyn Iterator<Item = Tuple> + Send>,
}

impl SeqScan {
    pub fn new(table: &Arc<dyn TableAm>) -> Self {
        Self { iter: table.scan() }
    }
}

impl ExecNode for SeqScan {
    fn next(&mut self) -> Option<Tuple> {
        self.iter.next()
    }
}

/// Column projection by index on top of a child node.
pub struct Project {
    child: Box<dyn ExecNode>,
    indices: Vec<usize>,
}

impl Project {
    pub fn new(child: Box<dyn ExecNode>, indices: Vec<usize>) -> Self {
        Self { child, indices }
    }
}

impl ExecNode for Project {
    fn next(&mut self) -> Option<Tuple> {
        let row = self.child.next()?;
        Some(self.indices.iter().map(|&i| row[i].clone()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_memory_storage::MemoryEngine;
    use crabgresql_storage_api::{Column, TableEngine, TableSchema};
    use crabgresql_types::Value;

    #[test]
    fn values_yields_rows_in_order() {
        let mut node = Values::new(vec![vec![Value::Int4(1)], vec![Value::Int4(2)]]);
        assert_eq!(node.next(), Some(vec![Value::Int4(1)]));
        assert_eq!(node.next(), Some(vec![Value::Int4(2)]));
        assert_eq!(node.next(), None);
    }

    #[test]
    fn seq_scan_reads_memory_table() {
        let engine = MemoryEngine::new();
        let table = engine
            .create_table(TableSchema {
                name: "t".into(),
                columns: vec![
                    Column {
                        name: "id".into(),
                        ty: PgType::Int4,
                    },
                    Column {
                        name: "label".into(),
                        ty: PgType::Text,
                    },
                ],
            })
            .unwrap();
        table.insert(vec![Value::Int4(7), Value::Text("seven".into())]);

        let mut scan = SeqScan::new(&table);
        assert_eq!(
            scan.next(),
            Some(vec![Value::Int4(7), Value::Text("seven".into())])
        );
        assert_eq!(scan.next(), None);
    }

    #[test]
    fn project_reorders_columns() {
        let child = Values::new(vec![vec![Value::Int4(1), Value::Text("a".into())]]);
        let mut node = Project::new(Box::new(child), vec![1, 0]);
        assert_eq!(
            node.next(),
            Some(vec![Value::Text("a".into()), Value::Int4(1)])
        );
    }
}
