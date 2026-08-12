//! Generated-column evaluation on the write paths.
//!
//! A relation stores its generation expressions as canonical SQL (see
//! [`crabgresql_storage_api::GeneratedColumn`]), so they have to be bound before
//! they can be evaluated. The binding is hoisted out of the per-row loop exactly
//! the way a [`crate::checks::CheckSet`]'s is, and for the same reason it cannot
//! move further up into the planner: an INSERT that routes into a partition only
//! learns which relation a row lands in mid-statement.
//!
//! Both kinds of generated column are *computed* here, including the virtual
//! ones. That is what lets NOT NULL and CHECK see a row's real values — upstream
//! enforces both against a virtual column too. What separates the two kinds is
//! what reaches storage: [`GeneratedSet::blank_virtual`] clears the virtual
//! slots again just before the write, because a virtual column stores nothing
//! and is recomputed by the binder wherever it is read.

use std::sync::Arc;

use crabgresql_binder::BoundExpr;
use crabgresql_storage_api::{Generation, TableSchema, Tuple, TypeCatalog};
use crabgresql_types::Value;

use crate::{ExecContext, ExecError, eval};

/// A relation's generated columns, bound and ready to evaluate against a row.
pub struct GeneratedSet {
    /// `(column position, kind, expression)`, in column order — which is also
    /// the order a row is filled in, so a `serial` column's sequence advances
    /// identically whether or not the relation has generated columns.
    entries: Vec<(usize, Generation, BoundExpr)>,
    /// Whether any entry is virtual, so the common case skips the second pass.
    has_virtual: bool,
}

impl GeneratedSet {
    /// Bind every generation expression of `schema`.
    pub fn for_schema(schema: &TableSchema, ctx: &ExecContext) -> Result<Self, ExecError> {
        // The overwhelmingly common case — a relation with no generated column
        // — pays one scan of the column list and never touches the catalog.
        if schema.columns.iter().all(|c| c.generated.is_none()) {
            return Ok(Self::none());
        }
        let Some(catalog) = ctx.types.clone() else {
            return Err(ExecError::new(
                "XX000",
                format!(
                    "no type catalog available to bind the generated columns of relation \"{}\"",
                    schema.name
                ),
            ));
        };
        let mut entries = Vec::new();
        let mut has_virtual = false;
        for (index, column) in schema.columns.iter().enumerate() {
            let Some(generated) = &column.generated else {
                continue;
            };
            has_virtual |= generated.kind == Generation::Virtual;
            entries.push((index, generated.kind, bind_stored(schema, index, &catalog)?));
        }
        Ok(GeneratedSet {
            entries,
            has_virtual,
        })
    }

    /// A set that generates nothing, for a relation that declares no generated
    /// column.
    pub fn none() -> Self {
        GeneratedSet {
            entries: Vec::new(),
            has_virtual: false,
        }
    }

    /// Whether this relation has any generated column at all. A caller that
    /// only needs the *values* can skip its own widening work when this holds.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Overwrite every generated slot of `tuple` with the value its expression
    /// produces. Whatever the statement put there is discarded: the write paths
    /// refuse a user-supplied value before this runs, so the slot holds only the
    /// NULL placeholder a column default left.
    ///
    /// A generation expression cannot reference another generated column (the
    /// DDL refuses it), so the order the slots are filled in cannot matter.
    pub fn compute(&self, tuple: &mut Tuple, ctx: &ExecContext) -> Result<(), ExecError> {
        for (index, _, expr) in &self.entries {
            tuple[*index] = eval(expr, tuple, ctx)?;
        }
        Ok(())
    }

    /// Clear the virtual slots, which store nothing. Called after the row has
    /// been validated and projected, immediately before it is written.
    pub fn blank_virtual(&self, tuple: &mut Tuple) {
        if !self.has_virtual {
            return;
        }
        for (index, kind, _) in &self.entries {
            if *kind == Generation::Virtual {
                tuple[*index] = Value::Null;
            }
        }
    }
}

/// Re-parse and re-bind one stored generation expression against `schema`.
///
/// As with a stored CHECK, the DDL path already proved this text binds, so a
/// failure here means the stored catalog and the running code disagree.
fn bind_stored(
    schema: &TableSchema,
    index: usize,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundExpr, ExecError> {
    crabgresql_binder::bind_stored_generation(schema, index, catalog).map_err(|e| {
        ExecError::new(
            "XX000",
            format!(
                "generation expression of column \"{}\" of relation \"{}\" cannot be bound: {}",
                schema.columns[index].name, schema.name, e.message
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabgresql_storage_api::{Column, EmptyTypeCatalog, GeneratedColumn};
    use crabgresql_types::PgType;

    fn schema_with(kind: Generation, expr: &str) -> TableSchema {
        let mut b = Column::new("b", PgType::Int4);
        b.generated = Some(GeneratedColumn {
            kind,
            expr: expr.to_string(),
        });
        TableSchema::new("t", vec![Column::new("a", PgType::Int4), b])
    }

    fn ctx() -> ExecContext {
        ExecContext {
            types: Some(Arc::new(EmptyTypeCatalog)),
            ..ExecContext::default()
        }
    }

    #[test]
    fn stored_column_is_computed_and_survives_the_blanking_pass() -> Result<(), ExecError> {
        let schema = schema_with(Generation::Stored, "(a * 2)");
        let set = GeneratedSet::for_schema(&schema, &ctx())?;
        let mut tuple = vec![Value::Int4(21), Value::Null];
        set.compute(&mut tuple, &ctx())?;
        assert_eq!(tuple[1], Value::Int4(42));
        set.blank_virtual(&mut tuple);
        assert_eq!(tuple[1], Value::Int4(42));
        Ok(())
    }

    #[test]
    fn virtual_column_is_computed_for_constraints_then_cleared() -> Result<(), ExecError> {
        let schema = schema_with(Generation::Virtual, "(a * 2)");
        let set = GeneratedSet::for_schema(&schema, &ctx())?;
        let mut tuple = vec![Value::Int4(21), Value::Null];
        set.compute(&mut tuple, &ctx())?;
        // The value exists for as long as NOT NULL / CHECK need to see it …
        assert_eq!(tuple[1], Value::Int4(42));
        // … and nothing of it reaches storage.
        set.blank_virtual(&mut tuple);
        assert_eq!(tuple[1], Value::Null);
        Ok(())
    }

    #[test]
    fn a_relation_without_generated_columns_binds_nothing() -> Result<(), ExecError> {
        let schema = TableSchema::new("t", vec![Column::new("a", PgType::Int4)]);
        // No catalog in the context: reaching the binder at all would fail here.
        let set = GeneratedSet::for_schema(&schema, &ExecContext::default())?;
        assert!(set.is_empty());
        Ok(())
    }
}
