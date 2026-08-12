//! Row-level `CHECK` constraint enforcement.
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
        let Some(catalog) = ctx.types.clone() else {
            return Err(ExecError::new(
                "XX000",
                format!(
                    "no type catalog available to bind the check constraints of relation \"{}\"",
                    schema.name
                ),
            ));
        };
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
