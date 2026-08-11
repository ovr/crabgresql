//! What still depends on a routine, for `DROP FUNCTION`.
//!
//! PostgreSQL keeps a `pg_depend` edge from every stored expression to the
//! objects it names, and refuses a `DROP` that would strand one. This tree has
//! no such catalog, so the edges are recovered on demand: the relations are
//! swept, each stored expression is re-bound, and the binder reports which user
//! routines the bind resolved through [`TypeCatalog::note_routine_use`].
//!
//! Re-binding rather than matching text, because only binding knows which
//! *overload* a call picked — `f(int)` may be dropped while a default calling
//! `f(text)` keeps working — and only binding sees a `LANGUAGE SQL` routine at
//! all, since it is inlined and leaves no OID in the bound tree.
//!
//! **This closes the DROP hole, not the dangling-reference hole.**
//! [`crate::global_catalog::GlobalCatalog`] is in-memory, so user routines do
//! not survive a restart and a stored expression naming one dangles again
//! afterwards regardless. Nothing here should be read as a durability guarantee.
//!
//! TODO: block a `DROP FUNCTION` whose routine a view body calls. PostgreSQL
//! blocks it and the same recorder would find it, but binding a whole query at
//! DROP time drags in view expansion and inheritance fan-out.
//!
//! TODO: block a `DROP TYPE`/`DROP CAST` reached from a stored expression.
//! That needs its own hooks and its own decision about what a column *of* a
//! user type contributes.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crabgresql_pg_wire::sqlstate;
use crabgresql_storage_api::{EnumInfo, TableEngine, TypeCatalog, UserCast, UserType};
use crabgresql_types::PgType;

use crate::error::PgError;
use crate::global_catalog::ResolvedRoutine;

/// One reason a routine cannot be dropped.
pub(crate) enum FuncDependent {
    Default {
        relation: String,
        column: String,
    },
    Check {
        relation: String,
        constraint: String,
    },
}

impl FuncDependent {
    /// The DETAIL line PostgreSQL prints for this dependent.
    fn describe(&self, routine: &str) -> String {
        match self {
            FuncDependent::Default { relation, column } => {
                format!(
                    "default value for column {column} of table {relation} depends on {routine}"
                )
            }
            FuncDependent::Check {
                relation,
                constraint,
            } => format!("constraint {constraint} on table {relation} depends on {routine}"),
        }
    }
}

/// A [`TypeCatalog`] that answers exactly like the one it wraps while recording
/// which user routines a bind resolved.
struct RecordingTypeCatalog {
    inner: Arc<dyn TypeCatalog>,
    /// A `Mutex` rather than a `Cell`, because `TypeCatalog` is `Send + Sync`.
    /// It is never contended: one bind at a time, on one thread.
    used: Mutex<BTreeSet<u32>>,
}

impl RecordingTypeCatalog {
    fn new(inner: &Arc<dyn TypeCatalog>) -> Self {
        RecordingTypeCatalog {
            inner: Arc::clone(inner),
            used: Mutex::new(BTreeSet::new()),
        }
    }

    /// The routines recorded since the last drain.
    fn take_used(&self) -> BTreeSet<u32> {
        std::mem::take(
            &mut *self
                .used
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")),
        )
    }
}

impl TypeCatalog for RecordingTypeCatalog {
    fn note_routine_use(&self, oid: u32) {
        self.used
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .insert(oid);
    }

    fn resolve_type(&self, name: &str) -> Option<UserType> {
        self.inner.resolve_type(name)
    }
    fn is_shell_type(&self, name: &str) -> bool {
        self.inner.is_shell_type(name)
    }
    fn user_type_name(&self, oid: u32) -> Option<String> {
        self.inner.user_type_name(oid)
    }
    fn find_cast(&self, source: PgType, target: PgType) -> Option<UserCast> {
        self.inner.find_cast(source, target)
    }
    fn backing_rep(&self, ty: PgType) -> PgType {
        self.inner.backing_rep(ty)
    }
    fn enum_info(&self, oid: u32) -> Option<EnumInfo> {
        self.inner.enum_info(oid)
    }
    fn routines(&self, name: &str) -> Vec<crabgresql_storage_api::RoutineSig> {
        self.inner.routines(name)
    }
}

/// Everything that would be stranded by dropping `targets`, paired with the
/// index of the target it depends on.
///
/// Cost is one parse+bind per stored expression per `DROP FUNCTION`, over every
/// relation in the engine. `DROP FUNCTION` is rare DDL and a relation with
/// neither a default nor a check costs nothing, so the sweep is deliberate
/// rather than optimized.
///
/// An expression that no longer binds still contributes whatever it recorded
/// **before** it failed. That is not a nicety: a routine is noted as it is
/// resolved and before its body is inlined, so `DEFAULT wrap()` — where `wrap`'s
/// body calls an already-dropped function — still reports `wrap`, which is what
/// PostgreSQL's eagerly-recorded edge does. Discarding the partial set instead
/// let a second `DROP` walk straight through the first one's damage.
///
/// Binding is never *skipped*, only truncated, so the worst case is a dependency
/// that goes unseen because the failure happened before the routine was reached
/// — under-reporting, never a spurious block.
pub(crate) fn routine_dependents(
    engine: &Arc<dyn TableEngine>,
    types: &Arc<dyn TypeCatalog>,
    targets: &[ResolvedRoutine],
) -> Vec<(usize, FuncDependent)> {
    let wanted: Vec<u32> = targets.iter().filter_map(|r| r.oid).collect();
    if wanted.is_empty() {
        return Vec::new();
    }
    let recorder = Arc::new(RecordingTypeCatalog::new(types));
    let as_catalog: Arc<dyn TypeCatalog> = Arc::clone(&recorder) as Arc<dyn TypeCatalog>;

    let mut relations = engine.relations();
    // Sorted so the DETAIL lines come out in a stable order. PostgreSQL orders by
    // dependent OID, which nothing here has — the same divergence `dependency_graph`
    // documents for views.
    relations.sort_by(|a, b| {
        a.namespace
            .cmp(&b.namespace)
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut found = Vec::new();
    for schema in &relations {
        for column in &schema.columns {
            let Some(sql) = &column.default else {
                continue;
            };
            let Some(expr) = crabgresql_binder::ruleutils::parse_expression(sql) else {
                continue;
            };
            // Result deliberately ignored: see the note above on partial binds.
            let _ = crabgresql_binder::bind_column_default(&expr, column, &as_catalog);
            for index in matched(&recorder.take_used(), targets) {
                found.push((
                    index,
                    FuncDependent::Default {
                        relation: schema.name.clone(),
                        column: column.name.clone(),
                    },
                ));
            }
        }
        for check in &schema.checks {
            let Some(expr) = crabgresql_binder::ruleutils::parse_expression(&check.expr) else {
                continue;
            };
            let _ = crabgresql_binder::bind_check_constraint(&expr, schema, &as_catalog);
            for index in matched(&recorder.take_used(), targets) {
                found.push((
                    index,
                    FuncDependent::Check {
                        relation: schema.name.clone(),
                        constraint: check.name.clone(),
                    },
                ));
            }
        }
    }
    found
}

/// The target positions this expression used, in statement order.
fn matched(used: &BTreeSet<u32>, targets: &[ResolvedRoutine]) -> Vec<usize> {
    targets
        .iter()
        .enumerate()
        .filter(|(_, r)| r.oid.is_some_and(|oid| used.contains(&oid)))
        .map(|(index, _)| index)
        .collect()
}

/// PostgreSQL's `2BP01` for a `DROP` that would strand a stored expression.
///
/// The message is singular when the statement named one object and plural when
/// it named several — PostgreSQL keys that off the *statement*, not off how many
/// targets actually have dependents. Probed against 18.4.
pub(crate) fn dependency_error(
    targets: &[ResolvedRoutine],
    deps: &[(usize, FuncDependent)],
) -> PgError {
    let message = match targets.len() {
        1 => format!(
            "cannot drop {} because other objects depend on it",
            targets[0].description
        ),
        _ => "cannot drop desired object(s) because other objects depend on them".to_string(),
    };
    let detail = deps
        .iter()
        .map(|(index, dep)| dep.describe(&targets[*index].description))
        .collect::<Vec<_>>()
        .join("\n");
    PgError::new(sqlstate::DEPENDENT_OBJECTS_STILL_EXIST, message)
        .with_detail(detail)
        .with_hint("Use DROP ... CASCADE to drop the dependent objects too.")
}
