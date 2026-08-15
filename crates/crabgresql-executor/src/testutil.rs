//! Fixtures shared by the executor's unit tests: a throwaway transaction
//! manager, seeded tables, and the row sources and collectors the node tests
//! drive.

use std::sync::Arc;

use crabgresql_binder::{BinOp, BoundExpr};
use crabgresql_storage_api::{
    Column, ColumnProjection, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, TableAm,
    TableEngine, TableSchema, Tuple,
};
use crabgresql_txn::{CommandId, TransactionManager, TxnContext, Xid};
use crabgresql_types::collation::DEFAULT_COLLATION_OID;
use crabgresql_types::{PgType, Value};

use crate::{ExecContext, ExecError, ExecNode, Projection, SeqScan, eval};

#[track_caller]
pub(crate) fn test_ok<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("test fixture operation failed: {error:?}"),
    }
}

thread_local! {
    /// One transaction manager per test thread (libtest runs each test on its
    /// own thread), so `wtxn`/`rtxn` share a commit log within a test but stay
    /// isolated across tests. These tests exercise executor mechanics, not MVCC
    /// visibility, so a write just commits immediately and a read sees it.
    static TM: TransactionManager = TransactionManager::new();
}

/// A committed writer context: seeds/DML whose versions are visible to any
/// later read. Commits the XID up front, so a row stamped with it (or an old
/// version it deletes) is immediately committed.
pub(crate) fn wtxn() -> TxnContext {
    TM.with(|tm| {
        let xid = tm.allocate_xid();
        test_ok(tm.commit(xid));
        tm.context(xid, CommandId::FIRST)
    })
}

/// A reader with no XID of its own and a fresh snapshot that sees every
/// committed version.
pub(crate) fn rtxn() -> TxnContext {
    TM.with(|tm| tm.context(Xid::INVALID, CommandId::FIRST))
}

pub(crate) fn int4(v: i32) -> BoundExpr {
    BoundExpr::Const {
        value: Value::Int4(v),
        ty: PgType::Int4,
    }
}

pub(crate) fn boolean(v: Option<bool>) -> BoundExpr {
    BoundExpr::Const {
        value: v.map(Value::Bool).unwrap_or(Value::Null),
        ty: PgType::Bool,
    }
}

pub(crate) fn binary(op: BinOp, arg_ty: PgType, left: BoundExpr, right: BoundExpr) -> BoundExpr {
    BoundExpr::Binary {
        op,
        arg_ty,
        collation: DEFAULT_COLLATION_OID,
        left: Box::new(left),
        right: Box::new(right),
    }
}

pub(crate) fn eval_const(expr: &BoundExpr) -> Result<Value, ExecError> {
    eval(expr, &[], &ExecContext::default())
}

pub(crate) fn test_table() -> Arc<dyn TableAm> {
    let engine = crabgresql_pg_engine::ephemeral_engine();
    let table = test_ok(engine.create_table(TableSchema::in_namespace(
        "t",
        "public",
        vec![
            Column::new("id", PgType::Int4),
            Column::new("label", PgType::Text),
        ],
    )));
    let txn = wtxn();
    test_ok(table.insert(vec![Value::Int4(1), Value::Text("one".into())], &txn));
    test_ok(table.insert(vec![Value::Int4(2), Value::Text("two".into())], &txn));
    test_ok(table.insert(vec![Value::Int4(3), Value::Null], &txn));
    table
}

pub(crate) fn collect(node: &mut dyn ExecNode) -> Vec<Tuple> {
    let mut rows = Vec::new();
    while let Some(row) = test_ok(node.next()) {
        rows.push(row);
    }
    rows
}

/// `test_table`'s rows plus a physical unique index on `id`.
pub(crate) fn indexed_table() -> Arc<dyn TableAm> {
    let engine = crabgresql_pg_engine::ephemeral_engine();
    let table = test_ok(engine.create_table(TableSchema::in_namespace(
        "t",
        "public",
        vec![
            Column::new("id", PgType::Int4),
            Column::new("label", PgType::Text),
        ],
    )));
    test_ok(engine.create_index(
        "public",
        "t",
        IndexMetadata {
            name: "t_id_key".into(),
            method: IndexMethod::BTree,
            keys: vec![IndexKey {
                column: 0,
                descending: false,
                nulls_first: false,
            }],
            unique: true,
            nulls_distinct: true,
            constraint: Some(IndexConstraint::Unique),
        },
    ));
    let txn = wtxn();
    test_ok(table.insert(vec![Value::Int4(1), Value::Text("one".into())], &txn));
    test_ok(table.insert(vec![Value::Int4(2), Value::Text("two".into())], &txn));
    test_ok(table.insert(vec![Value::Int4(3), Value::Null], &txn));
    table
}

/// Scan `t` (ids 1,2,3 in insertion order), keeping just the `id` column.
pub(crate) fn id_scan(table: &Arc<dyn TableAm>) -> Box<dyn ExecNode> {
    Box::new(Projection::new(
        Box::new(SeqScan::new(
            table,
            &rtxn(),
            &ColumnProjection::All,
            &ExecContext::default(),
        )),
        vec![BoundExpr::ColumnRef {
            index: 0,
            ty: PgType::Int4,
        }],
        ExecContext::default(),
    ))
}

pub(crate) fn ids(node: &mut dyn ExecNode) -> Vec<i32> {
    collect(node)
        .into_iter()
        .map(|row| match row[0] {
            Value::Int4(n) => n,
            ref other => panic!("expected int4, got {other:?}"),
        })
        .collect()
}

/// A source node that streams pre-built tuples, for exercising nodes that
/// consume arbitrary rows (Sort, Distinct) without going through storage.
pub(crate) struct VecSource {
    rows: std::vec::IntoIter<Tuple>,
}

impl VecSource {
    pub(crate) fn boxed(rows: Vec<Tuple>) -> Box<dyn ExecNode> {
        Box::new(Self {
            rows: rows.into_iter(),
        })
    }
}

impl ExecNode for VecSource {
    fn next(&mut self) -> Result<Option<Tuple>, ExecError> {
        Ok(self.rows.next())
    }
}
