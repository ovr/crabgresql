//! `CREATE DOMAIN` in expression context: binding a domain's constraints, and
//! wrapping a value on its way into one.
//!
//! PostgreSQL treats a domain as a distinct type over a base type, and enforces
//! its constraints wherever a value *enters* the domain — an explicit
//! `(-1)::posint` raises 23514 with no table in sight. Everything downstream of
//! that entry, though, resolves on the base: `pg_typeof(a)` on a `posint` column
//! is `posint` while `pg_typeof(a + 1)` is `integer` (probed on 18.4). The two
//! halves of that behaviour are the two halves of this module —
//! [`wrap_domain`] puts a value in, [`undomain`] takes it back out for operator
//! and function resolution.

use std::sync::Arc;

use crabgresql_storage_api::{Column, DomainInfo, TableSchema, TypeCatalog};
use crabgresql_types::PgType;

use crabgresql_pg_wire::sqlstate;

use crate::BindError;

use super::assign::{bind_check_constraint, parse_stored_expr};
use super::bound::{BoundDomain, BoundExpr};
use super::datatype::{apply_length, apply_length_cast};
use super::scope::{Binding, with_column_collation};

/// The domain `ty` names, or `None` when it is a builtin, an enum, or any other
/// user type.
pub(crate) fn domain_of(ty: PgType, catalog: &dyn TypeCatalog) -> Option<DomainInfo> {
    match ty {
        PgType::User(oid) => catalog.domain_info(oid),
        _ => None,
    }
}

/// Strip a domain off an already-bound expression so that operator and function
/// resolution sees the base type — PostgreSQL's `getBaseType` applied at the
/// point of use.
///
/// The wrapper is a `Coerce` to the base, which is a no-op at run time: the
/// value under a domain already *is* a base value, so `coerce_value` sees a type
/// it already has. What changes is [`BoundExpr::ty`], and with it every
/// overload the resolver considers.
/// The domain's collation travels with it. A domain is where a collation can be
/// declared — `CREATE DOMAIN ci AS text COLLATE "C"` — and stripping the type
/// without it would order the values byte-wise, the wrong answer for every ICU
/// collation.
pub(crate) fn undomain(expr: BoundExpr, catalog: &dyn TypeCatalog) -> BoundExpr {
    let ty = expr.ty();
    let Some(info) = domain_of(ty, catalog) else {
        return expr;
    };
    let base = catalog.base_type(ty);
    let stripped = BoundExpr::Coerce {
        expr: Box::new(expr),
        ty: base,
    };
    with_column_collation(stripped, domain_collation(&info, base))
}

/// The collation a domain's values carry, or `None` when it is the one the base
/// type would have taken anyway — an implicit wrapper for that adds a node and
/// says nothing.
pub(crate) fn domain_collation(info: &DomainInfo, base: PgType) -> Option<u32> {
    info.collation
        .filter(|oid| *oid != crabgresql_types::collation::type_collation(base))
}

/// [`undomain`] over a [`Binding`]. An unknown literal has no type to strip, so
/// it passes through untouched and keeps taking its type from context.
pub(crate) fn undomain_binding(binding: Binding, catalog: &dyn TypeCatalog) -> Binding {
    match binding {
        Binding::Typed(e) => Binding::Typed(undomain(e, catalog)),
        other => other,
    }
}

/// Wrap `expr` — already coerced to the type at the end of the domain's
/// `typbasetype` chain — in the node that enforces the domain.
///
/// The domain's type modifier is applied here rather than by the caller: a
/// `CREATE DOMAIN v AS varchar(3)` records the 3 on the type, so a column of `v`
/// carries `atttypmod = -1` and this is the only place that knows the length.
/// It runs *before* the constraints, as PostgreSQL does — `'abcd'` cast to a
/// `varchar(3)` domain with a `CHECK (length(VALUE) = 3)` passes, because the
/// value the check sees is the truncated one.
///
/// `explicit` picks which of the two length rules applies, and the difference is
/// observable: `SELECT 'abcd'::v3` truncates to `abc` while
/// `INSERT INTO t(b) VALUES ('abcd')` into a `v3` column raises
/// `value too long for type character varying(3)`. Probed on 18.4.
pub(crate) fn wrap_domain(
    expr: BoundExpr,
    info: &DomainInfo,
    catalog: &Arc<dyn TypeCatalog>,
    explicit: bool,
) -> Result<BoundExpr, BindError> {
    // The modifier comes from the chain, not from `info`: a domain over a
    // domain declares none of its own — see `TypeCatalog::base_typmod`.
    let (base, typmod) = catalog.base_type_and_typmod(PgType::User(info.oid));
    let expr = match explicit {
        true => apply_length_cast(expr, base, typmod)?,
        false => apply_length(expr, base, typmod)?,
    };
    Ok(BoundExpr::CoerceToDomain {
        expr: Box::new(expr),
        domain: Arc::new(bind_domain(info, catalog)?),
    })
}

/// Bind a domain's constraints — its own and every one it inherits down the
/// `typbasetype` chain — against a one-column shape whose column is the base
/// type, so `VALUE` in a predicate resolves to `ColumnRef { index: 0 }`.
///
/// The chain is **flattened**, base first, each level sorted by name, and the
/// whole set is attributed to the outermost domain. That is what PostgreSQL
/// reports: over `i1 CHECK (VALUE > 1)` → `i2 CHECK (VALUE > 2)` →
/// `i3 CONSTRAINT aaa CHECK (VALUE > 3)`, the value 0 raises `value for domain
/// i3 violates check constraint "i1_check"` — the *outer* domain's name with
/// the *inner* constraint's. `NOT NULL` is inherited the same way, and likewise
/// names the outer domain, even though `typnotnull` on that row stays false.
///
/// Re-binding the stored SQL on every use mirrors what the executor's
/// `CheckSet` does for a table's checks, and for the same reason: the catalog
/// stores text, not trees, so that it depends on neither parser nor binder.
pub(crate) fn bind_domain(
    info: &DomainInfo,
    catalog: &Arc<dyn TypeCatalog>,
) -> Result<BoundDomain, BindError> {
    let schema = value_shape(info, catalog);
    // Innermost first, which is the order the constraints run in.
    let mut chain = vec![info.clone()];
    while let Some(next) = domain_of(chain[0].base, catalog.as_ref()) {
        chain.insert(0, next);
    }
    let mut not_null = false;
    let mut checks = Vec::new();
    for level in &chain {
        not_null |= level.not_null;
        let mut level_checks = Vec::with_capacity(level.checks.len());
        for check in &level.checks {
            let parsed = parse_stored_expr(&check.expr, "domain constraint")?;
            let (bound, _) = bind_check_constraint(&parsed, &schema, catalog).map_err(|e| {
                BindError::new(
                    sqlstate::INTERNAL_ERROR,
                    format!(
                        "constraint \"{}\" of domain \"{}\" cannot be bound: {}",
                        check.name, level.name, e.message
                    ),
                )
            })?;
            let name: Arc<str> = Arc::from(check.name.as_str());
            level_checks.push((name, bound));
        }
        // Within one level, by name — see [`BoundDomain::checks`].
        level_checks.sort_by(|a, b| a.0.cmp(&b.0));
        checks.append(&mut level_checks);
    }
    Ok(BoundDomain {
        oid: info.oid,
        name: Arc::from(info.name.as_str()),
        not_null,
        checks,
    })
}

/// The one-column shape a domain's predicates bind against. The column is
/// spelled `value` because that is what `VALUE` normalizes to, and the relation
/// is named after the domain so that a predicate written `d.VALUE` — which
/// PostgreSQL rejects — is at least diagnosed against the right object.
pub(crate) fn value_shape(info: &DomainInfo, catalog: &Arc<dyn TypeCatalog>) -> TableSchema {
    TableSchema::new(
        info.name.clone(),
        vec![Column::new("value", catalog.base_type(info.base))],
    )
}
