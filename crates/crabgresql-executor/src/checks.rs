//! Row-level `NOT NULL` and `CHECK` constraint enforcement.
//!
//! A relation stores its checks as canonical SQL (see
//! [`crabgresql_storage_api::CheckConstraint`]), so they have to be bound before
//! they can be evaluated. Binding once per row would re-parse the same string
//! for every tuple of a bulk load, so the statement builds a [`CheckSet`] once —
//! in the same place, and with the same lifetime, as its `UniqueKeySet`.
//!
//! The binding cannot be hoisted into the planner: an INSERT that routes into a
//! partition, or an UPDATE that moves a row between inheritance children, only
//! learns which relation a row lands in mid-statement, and materialising every
//! leaf's schema up front would make a single-row INSERT cost O(partitions).

use std::sync::Arc;

use crabgresql_binder::BoundExpr;
use crabgresql_storage_api::{TableSchema, Tuple, TypeCatalog};
use crabgresql_types::Value;

use crate::{ExecContext, ExecError, display_tuple, eval};

/// The columns of a shape that reject NULL, as tuple indices in ascending order.
///
/// Collapsed once per statement for the same reason a [`CheckSet`] is bound
/// once: the answer is a property of the shape, not of the row. Walking
/// `schema.columns` per row to read one `bool` out of each [`Column`] strides
/// ~130 bytes per column — 14 KB a row on a 105-column load — where this list
/// is 4 bytes a column and empty for a relation that declares no `NOT NULL` at
/// all.
///
/// [`Column`]: crabgresql_storage_api::Column
pub(crate) struct NotNullSet {
    /// Ascending, which is the order PostgreSQL reports a row's first violation
    /// in — a row with two NULLs names the earlier column.
    columns: Vec<u32>,
}

impl NotNullSet {
    /// Every column of `schema` that rejects NULL.
    pub(crate) fn for_schema(schema: &TableSchema) -> Self {
        Self::for_schema_excluding(schema, &[])
    }

    /// The same, minus the columns a caller has already proven non-NULL for
    /// every row it is about to hand over (see [`crate::collect_insert_tuples`]).
    ///
    /// The list is always derived from the **live** schema and `verified` only
    /// subtracts from it, so a column that became `NOT NULL` after the statement
    /// was bound is still checked. `verified` is ascending, so the two lists
    /// merge in one pass.
    pub(crate) fn for_schema_excluding(schema: &TableSchema, verified: &[u32]) -> Self {
        let mut next = 0;
        let mut columns = Vec::new();
        for (index, column) in schema.columns.iter().enumerate() {
            let index = index as u32;
            while next < verified.len() && verified[next] < index {
                next += 1;
            }
            if column.nullable || verified.get(next) == Some(&index) {
                continue;
            }
            columns.push(index);
        }
        NotNullSet { columns }
    }

    /// Reject a row holding NULL in a column that does not accept one.
    pub(crate) fn validate(
        &self,
        schema: &TableSchema,
        tuple: &Tuple,
        ctx: &ExecContext,
    ) -> Result<(), ExecError> {
        for &index in &self.columns {
            // `get` rather than indexing: the predecessor zipped the two lists,
            // which silently stops at a tuple narrower than the shape. Preserved
            // rather than turned into a panic — nothing here is the place to
            // discover a short tuple.
            if matches!(tuple.get(index as usize), Some(Value::Null)) {
                return Err(violation(schema, tuple, index as usize, ctx));
            }
        }
        Ok(())
    }
}

/// Build the 23502. Out of line so the loop above stays a load and a compare:
/// this arm allocates two strings and renders the whole row.
#[cold]
#[inline(never)]
fn violation(schema: &TableSchema, tuple: &Tuple, index: usize, ctx: &ExecContext) -> ExecError {
    ExecError::new(
        "23502",
        format!(
            "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
            schema.columns[index].name, schema.name
        ),
    )
    .with_detail(Some(format!(
        "Failing row contains ({}).",
        display_tuple(schema, tuple, ctx)
    )))
}

/// A relation's `CHECK` constraints, bound and ordered the way violations are
/// reported.
pub(crate) struct CheckSet {
    /// `(constraint name, predicate)`, sorted by name.
    entries: Vec<(String, BoundExpr)>,
}

impl CheckSet {
    /// Bind every check of `schema`, ready to validate rows of that shape.
    ///
    /// Entries are sorted **by name**, which is the order PostgreSQL reports
    /// violations in — a row failing two checks names the alphabetically first
    /// one, regardless of which was declared first. Verified against PostgreSQL
    /// 18.4: `CONSTRAINT bbb CHECK (a > 100), CONSTRAINT aaa CHECK (a > 200)`
    /// with `a = 0` reports `aaa`.
    pub(crate) fn for_schema(schema: &TableSchema, ctx: &ExecContext) -> Result<Self, ExecError> {
        // The overwhelmingly common case — a relation with no checks — pays one
        // `is_empty()` and never touches the catalog.
        if schema.checks.is_empty() {
            return Ok(Self::none());
        }
        let catalog = require_types(ctx, schema, "check constraints")?;
        let mut entries = Vec::with_capacity(schema.checks.len());
        for check in &schema.checks {
            entries.push((check.name.clone(), bind_stored(schema, check, &catalog)?));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(CheckSet { entries })
    }

    /// A set that checks nothing, for a relation that declares no constraints.
    pub(crate) fn none() -> Self {
        CheckSet {
            entries: Vec::new(),
        }
    }

    /// Enforce every check against `tuple`.
    ///
    /// **A predicate that evaluates to NULL passes.** PostgreSQL rejects only a
    /// row whose check evaluates to `false`, which is why `CHECK (x > 3)` admits
    /// a NULL `x` — the single most important semantic detail here, and the one
    /// a `matches!(.., Value::Bool(true))` test would silently get wrong.
    pub(crate) fn validate(
        &self,
        schema: &TableSchema,
        tuple: &Tuple,
        ctx: &ExecContext,
    ) -> Result<(), ExecError> {
        for (name, predicate) in &self.entries {
            // Non-boolean is unreachable: the predicate was coerced to boolean
            // when it was bound, both at DDL time and again just now.
            if !matches!(eval(predicate, tuple, ctx)?, Value::Bool(false)) {
                continue;
            }
            return Err(ExecError::new(
                "23514",
                format!(
                    "new row for relation \"{}\" violates check constraint \"{name}\"",
                    schema.name
                ),
            )
            .with_detail(Some(format!(
                "Failing row contains ({}).",
                display_tuple(schema, tuple, ctx)
            ))));
        }
        Ok(())
    }
}

/// The type catalog a stored expression needs to re-bind, or the internal error
/// that says this context cannot bind one at all. Shared with
/// [`crate::generated::GeneratedSet`], which re-binds its own stored text under
/// exactly the same conditions; `what` names the set in the message.
pub(crate) fn require_types(
    ctx: &ExecContext,
    schema: &TableSchema,
    what: &str,
) -> Result<Arc<dyn TypeCatalog>, ExecError> {
    ctx.types.clone().ok_or_else(|| {
        ExecError::new(
            "XX000",
            format!(
                "no type catalog available to bind the {what} of relation \"{}\"",
                schema.name
            ),
        )
    })
}

/// Re-parse and re-bind one stored predicate against `schema`.
///
/// The DDL path already proved this text binds — it *came* from binding the
/// user's expression and deparsing the result — so a failure here means the
/// stored catalog and the running code disagree, which is an internal error
/// rather than anything the statement did wrong.
fn bind_stored(
    schema: &TableSchema,
    check: &crabgresql_storage_api::CheckConstraint,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundExpr, ExecError> {
    let parsed = crabgresql_binder::ruleutils::parse_expression(&check.expr).ok_or_else(|| {
        ExecError::new(
            "XX000",
            format!(
                "check constraint \"{}\" of relation \"{}\" is not a single expression",
                check.name, schema.name
            ),
        )
    })?;
    let (bound, _) =
        crabgresql_binder::bind_check_constraint(&parsed, schema, catalog).map_err(|e| {
            ExecError::new(
                "XX000",
                format!(
                    "check constraint \"{}\" of relation \"{}\" cannot be bound: {}",
                    check.name, schema.name, e.message
                ),
            )
        })?;
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_storage_api::{CheckConstraint, Column, EmptyTypeCatalog};
    use crabgresql_types::PgType;

    fn check(name: &str, expr: &str) -> CheckConstraint {
        CheckConstraint {
            name: name.to_string(),
            expr: expr.to_string(),
            columns: vec![0],
            validated: true,
            islocal: true,
            inhcount: 0,
        }
    }

    fn schema_with(checks: Vec<CheckConstraint>) -> TableSchema {
        let mut schema = TableSchema::new("t", vec![Column::new("x", PgType::Int4)]);
        schema.checks = checks;
        schema
    }

    fn ctx() -> ExecContext {
        ExecContext {
            types: Some(Arc::new(EmptyTypeCatalog)),
            ..ExecContext::default()
        }
    }

    #[test]
    fn a_false_predicate_is_a_check_violation() {
        let schema = schema_with(vec![check("t_x_check", "(x > 3)")]);
        let ctx = ctx();
        let set = CheckSet::for_schema(&schema, &ctx).expect("binds");
        let err = set
            .validate(&schema, &vec![Value::Int4(1)], &ctx)
            .expect_err("1 > 3 is false");
        assert_eq!(err.code, "23514");
        assert_eq!(
            err.message,
            "new row for relation \"t\" violates check constraint \"t_x_check\""
        );
        assert_eq!(err.detail.as_deref(), Some("Failing row contains (1)."));
    }

    #[test]
    fn a_true_predicate_passes() {
        let schema = schema_with(vec![check("t_x_check", "(x > 3)")]);
        let ctx = ctx();
        let set = CheckSet::for_schema(&schema, &ctx).expect("binds");
        assert!(set.validate(&schema, &vec![Value::Int4(9)], &ctx).is_ok());
    }

    /// The rule that separates a CHECK from a WHERE clause: PostgreSQL rejects
    /// only a predicate that evaluates to false, so an unknown result admits the
    /// row. `CHECK (x > 3)` therefore accepts a NULL `x`.
    #[test]
    fn a_null_predicate_passes() {
        let schema = schema_with(vec![check("t_x_check", "(x > 3)")]);
        let ctx = ctx();
        let set = CheckSet::for_schema(&schema, &ctx).expect("binds");
        assert!(set.validate(&schema, &vec![Value::Null], &ctx).is_ok());
    }

    /// Two violated checks name the alphabetically first one, not the one
    /// declared first — verified against PostgreSQL 18.4.
    #[test]
    fn violations_resolve_by_name_not_declaration_order() {
        let schema = schema_with(vec![check("bbb", "(x > 100)"), check("aaa", "(x > 200)")]);
        let ctx = ctx();
        let set = CheckSet::for_schema(&schema, &ctx).expect("binds");
        let err = set
            .validate(&schema, &vec![Value::Int4(0)], &ctx)
            .expect_err("0 violates both");
        assert!(
            err.message.ends_with("check constraint \"aaa\""),
            "expected the alphabetically first constraint, got: {}",
            err.message
        );
    }

    /// Three columns, the middle one nullable.
    fn notnull_schema() -> TableSchema {
        let mut schema = TableSchema::new(
            "t",
            vec![
                Column::new("a", PgType::Int4),
                Column::new("b", PgType::Int4),
                Column::new("c", PgType::Int4),
            ],
        );
        schema.columns[0].nullable = false;
        schema.columns[2].nullable = false;
        schema
    }

    #[test]
    fn a_null_in_a_not_null_column_is_rejected() {
        let schema = notnull_schema();
        let ctx = ExecContext::default();
        let set = NotNullSet::for_schema(&schema);
        let err = set
            .validate(
                &schema,
                &vec![Value::Int4(1), Value::Null, Value::Null],
                &ctx,
            )
            .expect_err("c rejects NULL");
        assert_eq!(err.code, "23502");
        assert_eq!(
            err.message,
            "null value in column \"c\" of relation \"t\" violates not-null constraint"
        );
        assert_eq!(
            err.detail.as_deref(),
            Some("Failing row contains (1, null, null).")
        );
        // The nullable column in the middle passes on its own.
        assert!(
            set.validate(
                &schema,
                &vec![Value::Int4(1), Value::Null, Value::Int4(3)],
                &ctx
            )
            .is_ok()
        );
    }

    /// A row violating two columns names the earlier one, as PostgreSQL does.
    #[test]
    fn the_first_violated_column_is_reported() {
        let schema = notnull_schema();
        let ctx = ExecContext::default();
        let err = NotNullSet::for_schema(&schema)
            .validate(&schema, &vec![Value::Null, Value::Null, Value::Null], &ctx)
            .expect_err("a and c both reject NULL");
        assert!(
            err.message.contains("column \"a\""),
            "expected the earlier column, got: {}",
            err.message
        );
    }

    /// The subtractive contract: `verified` only removes columns, and a column
    /// it does not name is still checked — which is what keeps a schema that
    /// gained a `NOT NULL` after the source was built safe.
    #[test]
    fn verified_columns_are_skipped_and_only_those() {
        let schema = notnull_schema();
        let ctx = ExecContext::default();
        let row = vec![Value::Null, Value::Null, Value::Null];

        let set = NotNullSet::for_schema_excluding(&schema, &[0]);
        let err = set
            .validate(&schema, &row, &ctx)
            .expect_err("c still fails");
        assert!(err.message.contains("column \"c\""), "{}", err.message);

        // Vouching for every not-null column leaves nothing to check. A column
        // that is nullable anyway may appear in the list without effect.
        let set = NotNullSet::for_schema_excluding(&schema, &[0, 1, 2]);
        assert!(set.validate(&schema, &row, &ctx).is_ok());
    }

    /// A tuple narrower than the shape stops where the values do — the behavior
    /// of the `zip` this replaced, kept so a short tuple is not a panic.
    #[test]
    fn a_short_tuple_checks_only_the_values_it_has() {
        let schema = notnull_schema();
        let ctx = ExecContext::default();
        let set = NotNullSet::for_schema(&schema);
        assert!(set.validate(&schema, &vec![Value::Int4(1)], &ctx).is_ok());
    }

    #[test]
    fn a_relation_with_no_checks_needs_no_catalog() {
        let schema = schema_with(Vec::new());
        // Deliberately a context with no type catalog: the empty case must not
        // reach for one, or every non-executing context would have to carry it.
        let ctx = ExecContext::default();
        let set = CheckSet::for_schema(&schema, &ctx).expect("no checks, no catalog needed");
        assert!(set.validate(&schema, &vec![Value::Int4(1)], &ctx).is_ok());
    }
}
