//! Planner: logical plan → physical plan.
//!
//! With one access path (a full scan) the mapping is 1:1 — this crate exists
//! to hold the layer boundary where index selection, join ordering and
//! cost-based choices land later (docs/ARCHITECTURE.md §2).

use std::sync::Arc;

use crabgresql_binder::{
    AggInput, BinOp, BoundAggregate, BoundExpr, JoinExpr, JoinInput, JoinKind, LogicalPlan,
    OutputColumn, SortKey, TableFn,
};
use crabgresql_storage_api::TableAm;
use crabgresql_types::PgType;

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
    /// Recursive joined row source, then the standard Filter → Projection →
    /// Sort tail. Mirrors [`LogicalPlan::Join`].
    Join {
        source: PhysicalJoinExpr,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        predicate: Option<BoundExpr>,
        sort: Vec<SortKey>,
    },
    /// Grouped aggregation. Mirrors [`LogicalPlan::Aggregate`]: the executor
    /// filters `input` by `predicate`, groups by `group_exprs`, accumulates the
    /// `aggregates`, filters groups by `having`, then runs the standard
    /// projection/sort tail.
    Aggregate {
        input: PhysicalAggInput,
        predicate: Option<BoundExpr>,
        group_exprs: Vec<BoundExpr>,
        aggregates: Vec<BoundAggregate>,
        having: Option<BoundExpr>,
        columns: Vec<OutputColumn>,
        projections: Vec<BoundExpr>,
        sort: Vec<SortKey>,
    },
    /// LIMIT/OFFSET above a source plan (after its sort). Mirrors
    /// [`LogicalPlan::Limit`].
    Limit {
        source: Box<PhysicalPlan>,
        limit: Option<i64>,
        offset: Option<i64>,
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

/// One equi-join key of a hash join: a pair of expressions, one addressing the
/// left input and one the right, that must compare equal. Both evaluate to `ty`
/// (the operands' promoted comparison type), so the hash/equality of the two
/// sides is computed under a single type. Extracted from `col = col` conjuncts
/// of the ON predicate (see [`extract_hash_keys`]).
///
/// The operand expressions still index into the *concatenated* `left || right`
/// row (the form the binder produced): `left` references `[0, left_width)` and
/// `right` references `[left_width, ..)`. The executor evaluates each against a
/// padded row so the indices stay valid.
#[derive(Clone, Debug, PartialEq)]
pub struct HashKey {
    pub left: BoundExpr,
    pub right: BoundExpr,
    pub ty: PgType,
}

/// A [`JoinExpr`] whose leaf subplans have already been physically planned.
pub enum PhysicalJoinExpr {
    Input {
        input: PhysicalJoinInput,
        width: usize,
    },
    /// A binary join. When `hash_keys` is non-empty the executor runs a hash
    /// join keyed on them and `predicate` holds only the residual (non-equi)
    /// conjuncts of the ON clause; when empty it runs a nested-loop join and
    /// `predicate` is the whole ON condition (`None` for CROSS/comma joins).
    Join {
        left: Box<PhysicalJoinExpr>,
        right: Box<PhysicalJoinExpr>,
        kind: JoinKind,
        predicate: Option<BoundExpr>,
        hash_keys: Vec<HashKey>,
    },
}

impl PhysicalJoinExpr {
    pub fn width(&self) -> usize {
        match self {
            PhysicalJoinExpr::Input { width, .. } => *width,
            PhysicalJoinExpr::Join { left, right, .. } => left.width() + right.width(),
        }
    }
}

/// The row source of a [`PhysicalPlan::Aggregate`], mirroring [`AggInput`].
pub enum PhysicalAggInput {
    Scan(Arc<dyn TableAm>),
    Join(PhysicalJoinExpr),
    SingleRow,
}

fn plan_join_input(input: JoinInput) -> PhysicalJoinInput {
    match input {
        JoinInput::Scan(table) => PhysicalJoinInput::Scan(table),
        JoinInput::Subplan(source) => PhysicalJoinInput::Subplan(Box::new(plan(*source))),
        JoinInput::TableFunction { func, args } => PhysicalJoinInput::TableFunction { func, args },
    }
}

fn plan_join_expr(source: JoinExpr) -> PhysicalJoinExpr {
    match source {
        JoinExpr::Input { input, width } => PhysicalJoinExpr::Input {
            input: plan_join_input(input),
            width,
        },
        JoinExpr::Join {
            left,
            right,
            kind,
            predicate,
        } => {
            let left_width = left.width();
            let (hash_keys, predicate) = extract_hash_keys(predicate, left_width);
            PhysicalJoinExpr::Join {
                left: Box::new(plan_join_expr(*left)),
                right: Box::new(plan_join_expr(*right)),
                kind,
                predicate,
                hash_keys,
            }
        }
    }
}

/// Split an ON predicate into hash-join keys plus a residual filter.
///
/// The predicate is a boolean over the concatenated `left || right` row, where
/// the left input occupies column indices `[0, left_width)`. Any top-level
/// (AND-connected) conjunct of the form `<left-only expr> = <right-only expr>`
/// (in either operand order) becomes a [`HashKey`]; every other conjunct — a
/// non-equality, a same-side equality (`a.x = a.z`), or one comparing against a
/// constant (`a.x = 5`) — stays in the residual predicate, re-AND-ed together.
///
/// With no extractable key the residual equals the original predicate, so the
/// executor falls back to a nested-loop join with identical semantics.
fn extract_hash_keys(
    predicate: Option<BoundExpr>,
    left_width: usize,
) -> (Vec<HashKey>, Option<BoundExpr>) {
    let Some(predicate) = predicate else {
        return (Vec::new(), None);
    };
    let mut conjuncts = Vec::new();
    flatten_and(predicate, &mut conjuncts);

    let mut hash_keys = Vec::new();
    let mut residual = Vec::new();
    for conjunct in conjuncts {
        match as_equi_key(conjunct, left_width) {
            Ok(key) => hash_keys.push(key),
            Err(other) => residual.push(other),
        }
    }
    (hash_keys, rebuild_and(residual))
}

/// Recursively split a top-level `AND` tree into its conjuncts, preserving order.
fn flatten_and(expr: BoundExpr, out: &mut Vec<BoundExpr>) {
    match expr {
        BoundExpr::Binary {
            op: BinOp::And,
            left,
            right,
            ..
        } => {
            flatten_and(*left, out);
            flatten_and(*right, out);
        }
        other => out.push(other),
    }
}

/// Re-combine conjuncts with `AND`, yielding `None` for an empty list.
fn rebuild_and(mut conjuncts: Vec<BoundExpr>) -> Option<BoundExpr> {
    let mut acc = conjuncts.pop()?;
    while let Some(next) = conjuncts.pop() {
        acc = BoundExpr::Binary {
            op: BinOp::And,
            arg_ty: PgType::Bool,
            left: Box::new(next),
            right: Box::new(acc),
        };
    }
    Some(acc)
}

/// If `conjunct` is a cross-side equality, return its [`HashKey`] (oriented so
/// `left` addresses the left input); otherwise hand the expression back as a
/// residual conjunct.
fn as_equi_key(conjunct: BoundExpr, left_width: usize) -> Result<HashKey, BoundExpr> {
    // Classify on a borrow so the non-key paths can return `conjunct` untouched
    // (no field-by-field rebuild). Only a cross-side equality on a type that
    // hashes distinctly qualifies; a poorly-hashed type (interval/inet/…) would
    // collapse the whole build side into one bucket, so keep it as a residual and
    // let the executor use a nested loop instead.
    let BoundExpr::Binary {
        op: BinOp::Eq,
        arg_ty,
        left,
        right,
    } = &conjunct
    else {
        return Err(conjunct);
    };
    if !arg_ty.hashes_distinctly() {
        return Err(conjunct);
    }
    let swap = match (ref_side(left, left_width), ref_side(right, left_width)) {
        (Some(Side::Left), Some(Side::Right)) => false,
        (Some(Side::Right), Some(Side::Left)) => true,
        _ => return Err(conjunct),
    };
    let ty = *arg_ty;
    // Confirmed a cross-side hashable equality — now consume and orient so
    // `left` addresses the left input.
    let BoundExpr::Binary { left, right, .. } = conjunct else {
        unreachable!("matched as an Eq binary above");
    };
    Ok(if swap {
        HashKey {
            left: *right,
            right: *left,
            ty,
        }
    } else {
        HashKey {
            left: *left,
            right: *right,
            ty,
        }
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Left,
    Right,
}

/// Which input a key operand references, or `None` if it spans both inputs or
/// references no column at all (a constant/param — not a join key). Column
/// indices below `left_width` are on the left, the rest on the right; the
/// operand's whole [`BoundExpr::column_ref_bounds`] range must fall on one side.
fn ref_side(expr: &BoundExpr, left_width: usize) -> Option<Side> {
    let (lo, hi) = expr.column_ref_bounds()?;
    if hi < left_width {
        Some(Side::Left)
    } else if lo >= left_width {
        Some(Side::Right)
    } else {
        None
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
            source,
            columns,
            projections,
            predicate,
            sort,
        } => PhysicalPlan::Join {
            source: plan_join_expr(source),
            columns,
            projections,
            predicate,
            sort,
        },
        LogicalPlan::Aggregate {
            input,
            predicate,
            group_exprs,
            aggregates,
            having,
            columns,
            projections,
            sort,
        } => PhysicalPlan::Aggregate {
            input: match input {
                AggInput::Scan(table) => PhysicalAggInput::Scan(table),
                AggInput::Join(source) => PhysicalAggInput::Join(plan_join_expr(source)),
                AggInput::SingleRow => PhysicalAggInput::SingleRow,
            },
            predicate,
            group_exprs,
            aggregates,
            having,
            columns,
            projections,
            sort,
        },
        LogicalPlan::Limit {
            source,
            limit,
            offset,
        } => PhysicalPlan::Limit {
            source: Box::new(plan(*source)),
            limit,
            offset,
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
        if let Err(error) = engine.create_table(TableSchema {
            name: "t".into(),
            columns: vec![
                Column::new("id", PgType::Int4),
                Column::new("big", PgType::Int8),
                Column::new("name", PgType::Text),
            ],
        }) {
            panic!("failed to create planner test table: {error}");
        }
        let stmts = match crabgresql_parser::parse(sql) {
            Ok(stmts) => stmts,
            Err(error) => panic!("invalid SQL test fixture `{sql}`: {error}"),
        };
        let logical = match match &stmts[0] {
            ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
            ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
            ast::Statement::Update(u) => bind_update(&engine, &catalog, u),
            ast::Statement::Delete(d) => bind_delete(&engine, &catalog, d),
            other => panic!("unexpected statement: {other}"),
        } {
            Ok(plan) => plan,
            Err(error) => panic!("failed to bind planner test SQL `{sql}`: {error}"),
        };
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
        let PhysicalPlan::Join {
            source, columns, ..
        } = plan_sql("SELECT * FROM t, (VALUES (1)) v(x)")
        else {
            panic!("expected Join");
        };
        assert!(matches!(
            source,
            PhysicalJoinExpr::Join {
                kind: JoinKind::Cross,
                predicate: None,
                ..
            }
        ));
        // t's three columns plus v's one.
        assert_eq!(columns.len(), 4);
    }

    #[test]
    fn outer_join_and_aggregate_input_map_recursively() {
        let PhysicalPlan::Aggregate {
            input: PhysicalAggInput::Join(source),
            ..
        } = plan_sql("SELECT count(*) FROM t a FULL JOIN t b ON a.id = b.id")
        else {
            panic!("expected Aggregate over Join");
        };
        // `a.id = b.id` is a cross-side equality, so it lowers to a hash join
        // with one key and no residual predicate.
        let PhysicalJoinExpr::Join {
            kind,
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join");
        };
        assert_eq!(kind, JoinKind::Full);
        assert!(predicate.is_none());
        assert_eq!(hash_keys.len(), 1);
    }

    #[test]
    fn equi_join_extracts_hash_key_and_leaves_residual() {
        // `a.id = b.id` is the sole key; `a.big > b.big` is a non-equi residual.
        let PhysicalPlan::Join { source, .. } =
            plan_sql("SELECT * FROM t a INNER JOIN t b ON a.id = b.id AND a.big > b.big")
        else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join {
            kind,
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert_eq!(kind, JoinKind::Inner);
        assert_eq!(hash_keys.len(), 1);
        assert_eq!(hash_keys[0].ty, PgType::Int4);
        // The residual keeps the `>` comparison as a single conjunct.
        assert!(matches!(
            predicate,
            Some(BoundExpr::Binary { op: BinOp::Gt, .. })
        ));
    }

    #[test]
    fn non_equi_and_constant_equalities_stay_nested_loop() {
        // `a.id = 1` compares a column to a constant (a filter, not a join key),
        // and `a.big < b.big` is non-equi: neither yields a hash key.
        let PhysicalPlan::Join { source, .. } =
            plan_sql("SELECT * FROM t a INNER JOIN t b ON a.id = 1 AND a.big < b.big")
        else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join {
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert!(hash_keys.is_empty());
        assert!(predicate.is_some());
    }

    #[test]
    fn equi_join_on_poorly_hashed_type_stays_nested_loop() {
        // `interval` equality is orderable (so the join binds) but agg::hash_key
        // can't distinguish interval values — they'd all share one bucket — so the
        // planner must keep it as a nested-loop predicate, not a hash key.
        let PhysicalPlan::Join { source, .. } = plan_sql(
            "SELECT * FROM (VALUES ('1 day'::interval)) a(x) \
             JOIN (VALUES ('1 day'::interval)) b(y) ON a.x = b.y",
        ) else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join {
            predicate,
            hash_keys,
            ..
        } = source
        else {
            panic!("expected Join node");
        };
        assert!(hash_keys.is_empty(), "interval key must not be hashed");
        assert!(predicate.is_some());
    }

    #[test]
    fn equi_join_on_hashable_nonint_type_extracts_key() {
        // `money` compares by its raw i64 and is now hashed distinctly, so an
        // equality on it is a hash key.
        let PhysicalPlan::Join { source, .. } = plan_sql(
            "SELECT * FROM (VALUES ('$1'::money)) a(x) \
             JOIN (VALUES ('$1'::money)) b(y) ON a.x = b.y",
        ) else {
            panic!("expected Join");
        };
        let PhysicalJoinExpr::Join { hash_keys, .. } = source else {
            panic!("expected Join node");
        };
        assert_eq!(hash_keys.len(), 1);
        assert_eq!(hash_keys[0].ty, PgType::Money);
    }

    #[test]
    fn aggregate_query_maps_to_physical_aggregate() {
        let PhysicalPlan::Aggregate {
            group_exprs,
            aggregates,
            projections,
            ..
        } = plan_sql("SELECT count(*) FROM t")
        else {
            panic!("expected Aggregate");
        };
        assert!(group_exprs.is_empty());
        assert_eq!(aggregates.len(), 1);
        assert_eq!(
            projections,
            vec![BoundExpr::ColumnRef {
                index: 0,
                ty: PgType::Int8
            }]
        );
    }

    #[test]
    fn grouped_aggregate_maps_to_physical_aggregate() {
        let PhysicalPlan::Aggregate {
            group_exprs,
            aggregates,
            ..
        } = plan_sql("SELECT id, count(*) FROM t GROUP BY id")
        else {
            panic!("expected Aggregate");
        };
        assert_eq!(group_exprs.len(), 1);
        assert_eq!(aggregates.len(), 1);
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
