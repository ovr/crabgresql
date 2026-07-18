//! Planner: logical plan → physical plan.
//!
//! With one access path (a full scan) the mapping is 1:1 — this crate exists
//! to hold the layer boundary where index selection, join ordering and
//! cost-based choices land later (docs/ARCHITECTURE.md §2).

use std::sync::Arc;

use crabgresql_binder::{BoundExpr, JoinInput, LogicalPlan, OutputColumn, SortKey, TableFn};
use crabgresql_storage_api::TableAm;

/// An executable plan. `Select` describes the SeqScan → Filter → Projection →
/// Sort pipeline the executor builds.
pub enum PhysicalPlan {
    Values {
        columns: Vec<OutputColumn>,
        rows: Vec<Vec<BoundExpr>>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    Select {
        table: Arc<dyn TableAm>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    Subquery {
        source: Box<PhysicalPlan>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    TableFunction {
        func: TableFn,
        args: Vec<BoundExpr>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    /// Cartesian product of two or more inputs, then the standard
    /// Filter → Projection → Sort tail. Mirrors [`LogicalPlan::Join`].
    Join {
        inputs: Vec<PhysicalJoinInput>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    Insert {
        table: Arc<dyn TableAm>,
        rows: Vec<Vec<BoundExpr>>,
    },
    Update {
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
        assignments: Vec<(usize, BoundExpr)>,
    },
    Delete {
        table: Arc<dyn TableAm>,
        predicate: Option<BoundExpr>,
    },
}

/// A join input, mirroring [`JoinInput`] but with the subplan already lowered
/// to a [`PhysicalPlan`].
pub enum PhysicalJoinInput {
    Scan(Arc<dyn TableAm>),
    Subplan(Box<PhysicalPlan>),
    TableFunction { func: TableFn, args: Vec<BoundExpr> },
}

fn plan_join_input(input: JoinInput) -> PhysicalJoinInput {
    match input {
        JoinInput::Scan(table) => PhysicalJoinInput::Scan(table),
        JoinInput::Subplan(source) => PhysicalJoinInput::Subplan(Box::new(plan(*source))),
        JoinInput::TableFunction { func, args } => {
            PhysicalJoinInput::TableFunction { func, args }
        }
    }
}

pub fn plan(logical: LogicalPlan) -> PhysicalPlan {
    match logical {
        LogicalPlan::Values {
            columns,
            rows,
            predicate,
            sort,
        } => PhysicalPlan::Values {
            columns,
            rows,
            predicate,
            sort,
        },
        LogicalPlan::Query {
            table,
            columns,
            projections,
            predicate,
            sort,
        } => PhysicalPlan::Select {
            table,
            columns,
            projections,
            predicate,
            sort,
        },
        LogicalPlan::Subquery {
            source,
            columns,
            projections,
            predicate,
            sort,
        } => PhysicalPlan::Subquery {
            source: Box::new(plan(*source)),
            columns,
            projections,
            predicate,
            sort,
        },
        LogicalPlan::TableFunction {
            func,
            args,
            columns,
            projections,
            predicate,
            sort,
        } => PhysicalPlan::TableFunction {
            func,
            args,
            columns,
            projections,
            predicate,
            sort,
        },
        LogicalPlan::Join {
            inputs,
            columns,
            projections,
            predicate,
            sort,
        } => PhysicalPlan::Join {
            inputs: inputs.into_iter().map(plan_join_input).collect(),
            columns,
            projections,
            predicate,
            sort,
        },
        LogicalPlan::Insert { table, rows } => PhysicalPlan::Insert { table, rows },
        LogicalPlan::Update {
            table,
            predicate,
            assignments,
        } => PhysicalPlan::Update {
            table,
            predicate,
            assignments,
        },
        LogicalPlan::Delete { table, predicate } => PhysicalPlan::Delete { table, predicate },
    }
}

#[cfg(test)]
mod tests {
    //! SQL in, physical plan out: parse → bind (against a memory table) →
    //! plan, asserting on the plan's structure.

    use super::*;
    use crabgresql_binder::{BinOp, bind_delete, bind_insert, bind_query, bind_update};
    use crabgresql_memory_storage::MemoryEngine;
    use crabgresql_parser::ast;
    use crabgresql_storage_api::{Column, EmptyTypeCatalog, TableEngine, TableSchema, TypeCatalog};
    use crabgresql_types::{PgType, Value};

    fn plan_sql(sql: &str) -> PhysicalPlan {
        let engine: Arc<dyn TableEngine> = Arc::new(MemoryEngine::new());
        let catalog: Arc<dyn TypeCatalog> = Arc::new(EmptyTypeCatalog);
        engine
            .create_table(TableSchema {
                name: "t".into(),
                columns: vec![
                    Column::new("id", PgType::Int4),
                    Column::new("big", PgType::Int8),
                    Column::new("name", PgType::Text),
                ],
            })
            .unwrap();
        let stmts = crabgresql_parser::parse(sql).unwrap();
        let logical = match &stmts[0] {
            ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
            ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
            ast::Statement::Update(u) => bind_update(&engine, &catalog, u),
            ast::Statement::Delete(d) => bind_delete(&engine, &catalog, d),
            other => panic!("unexpected statement: {other}"),
        }
        .unwrap();
        plan(logical)
    }

    #[test]
    fn select_one_becomes_values() {
        let PhysicalPlan::Values {
            columns,
            rows,
            predicate,
            ..
        } = plan_sql("SELECT 1")
        else {
            panic!("expected Values");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "?column?");
        assert_eq!(columns[0].ty, PgType::Int4);
        assert_eq!(
            rows,
            vec![vec![BoundExpr::Const {
                value: Value::Int4(1),
                ty: PgType::Int4
            }]]
        );
        assert!(predicate.is_none());
    }

    #[test]
    fn filtered_scan_becomes_select_with_predicate() {
        let PhysicalPlan::Select {
            columns,
            projections,
            predicate,
            ..
        } = plan_sql("SELECT * FROM t WHERE id = 1")
        else {
            panic!("expected Select");
        };
        // `*` expands to one ColumnRef per schema column.
        assert_eq!(columns.len(), 3);
        assert_eq!(
            projections[0],
            BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int4
            }
        );
        let Some(BoundExpr::Binary {
            op: BinOp::Eq,
            arg_ty: PgType::Int4,
            ..
        }) = predicate
        else {
            panic!("expected int4 equality predicate");
        };
    }

    #[test]
    fn expression_projection_is_named_column() {
        let PhysicalPlan::Select {
            columns,
            projections,
            ..
        } = plan_sql("SELECT id + 1 FROM t")
        else {
            panic!("expected Select");
        };
        assert_eq!(columns[0].name, "?column?");
        assert_eq!(columns[0].ty, PgType::Int4);
        let BoundExpr::Binary {
            op: BinOp::Add,
            arg_ty: PgType::Int4,
            ..
        } = &projections[0]
        else {
            panic!("expected addition projection");
        };
    }

    #[test]
    fn mixed_width_comparison_coerces_int4_side() {
        let PhysicalPlan::Select { predicate, .. } = plan_sql("SELECT id FROM t WHERE id < big")
        else {
            panic!("expected Select");
        };
        let Some(BoundExpr::Binary {
            op: BinOp::Lt,
            arg_ty: PgType::Int8,
            left,
            ..
        }) = predicate
        else {
            panic!("expected int8 comparison");
        };
        let BoundExpr::Coerce {
            ty: PgType::Int8, ..
        } = *left
        else {
            panic!("expected Coerce around the int4 side");
        };
    }

    #[test]
    fn update_plan_carries_indexed_assignments() {
        let PhysicalPlan::Update {
            assignments,
            predicate,
            ..
        } = plan_sql("UPDATE t SET name = 'x' WHERE id = 2")
        else {
            panic!("expected Update");
        };
        assert_eq!(
            assignments,
            vec![(
                2,
                BoundExpr::Const {
                    value: Value::Text("x".into()),
                    ty: PgType::Text
                }
            )]
        );
        assert!(predicate.is_some());
    }

    #[test]
    fn unfiltered_delete_has_no_predicate() {
        let PhysicalPlan::Delete { predicate, .. } = plan_sql("DELETE FROM t") else {
            panic!("expected Delete");
        };
        assert!(predicate.is_none());
    }

    #[test]
    fn cross_join_maps_to_physical_join() {
        let PhysicalPlan::Join { inputs, columns, .. } =
            plan_sql("SELECT * FROM t, (VALUES (1)) v(x)")
        else {
            panic!("expected Join");
        };
        assert_eq!(inputs.len(), 2);
        // t's three columns plus v's one.
        assert_eq!(columns.len(), 4);
    }

    #[test]
    fn insert_rows_are_prebound_constants() {
        let PhysicalPlan::Insert { rows, .. } = plan_sql("INSERT INTO t (id) VALUES (1), (2)")
        else {
            panic!("expected Insert");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[1][0],
            BoundExpr::Const {
                value: Value::Int4(2),
                ty: PgType::Int4
            }
        );
    }
}
