//! Collation derivation: which collation a bound expression carries, and which
//! one a comparison or sort over it should use.
//!
//! PostgreSQL gives every expression of a collatable type a collation and a
//! *derivation strength*. A `COLLATE` clause is **explicit** and overrides
//! everything below it; a column contributes its own collation **implicitly**;
//! a literal or a non-string input contributes **none**. Combining inputs takes
//! the strongest, and two conflicting explicit collations are an error.
//!
//! TODO: raise `42P22` when two *implicit* collations conflict, the way
//! PostgreSQL does at the point of use; here such an expression falls back to
//! the database collation instead. Explicit conflicts — the case a query
//! author can actually see and fix — do raise `42P22`.

use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::collation::{
    DEFAULT_COLLATION_OID, lookup_by_name, lookup_by_oid, type_collation,
};

use crate::BindError;
use crate::expr::BoundExpr;

/// How firmly an expression asserts its collation. Ordered weakest to
/// strongest: a stronger derivation wins when inputs are combined.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    /// No collation of its own — a literal, a parameter, or a non-string value.
    None,
    /// A column's declared collation.
    Implicit,
    /// An explicit `COLLATE` clause.
    Explicit,
}

/// A collation together with how strongly the expression asserts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Derived {
    pub collation: u32,
    pub strength: Strength,
}

impl Derived {
    /// No collation asserted: the database default, contributed by nothing.
    pub const NONE: Derived = Derived {
        collation: DEFAULT_COLLATION_OID,
        strength: Strength::None,
    };

    fn implicit(collation: u32) -> Derived {
        Derived {
            collation,
            strength: Strength::Implicit,
        }
    }

    fn explicit(collation: u32) -> Derived {
        Derived {
            collation,
            strength: Strength::Explicit,
        }
    }
}

/// The collation `expr` carries.
///
/// Follows PostgreSQL's rule that a function or operator result takes the
/// common collation of its collatable inputs, so `lower(x)`, `x || y`, and a
/// `CASE` over collated branches all propagate rather than losing the
/// collation. Nodes whose result is not collatable contribute nothing.
pub fn expr_collation(expr: &BoundExpr) -> Derived {
    // Only a string-valued expression has a collation at all. This also stops
    // propagation at the right places by itself: a comparison yields bool, and
    // a cast away from text drops the operand's collation.
    if !expr.ty().is_collatable() {
        return Derived::NONE;
    }
    match expr {
        BoundExpr::Collate {
            collation,
            explicit,
            ..
        } => {
            if *explicit {
                Derived::explicit(*collation)
            } else {
                Derived::implicit(*collation)
            }
        }
        // A column with no explicit COLLATE contributes its type's collation
        // implicitly — an unwrapped ColumnRef is exactly that case.
        BoundExpr::ColumnRef { ty, .. } | BoundExpr::OuterColumnRef { ty, .. } => {
            Derived::implicit(type_collation(*ty))
        }
        // Value-preserving wrappers pass their operand's collation through.
        BoundExpr::Coerce { expr, .. } | BoundExpr::Reinterpret { expr, .. } => {
            expr_collation(expr)
        }
        // A result takes the common collation of its collatable inputs.
        BoundExpr::FuncCall { args, .. } => combine_all(args),
        // A routine's result carries its declared type's default collation, not
        // its arguments' — the body is opaque, so nothing is derived through it.
        BoundExpr::Routine { .. } => Derived::NONE,
        BoundExpr::Binary { left, right, .. } => {
            expr_collation(left).max_with(expr_collation(right))
        }
        BoundExpr::Case { whens, else_, .. } => {
            let mut derived = Derived::NONE;
            for (_, result) in whens {
                derived = derived.max_with(expr_collation(result));
            }
            if let Some(else_) = else_ {
                derived = derived.max_with(expr_collation(else_));
            }
            derived
        }
        BoundExpr::ArrayCtor { elems, .. } => combine_all(elems),
        // Whichever argument answers non-NULL is the result, so every one of
        // them contributes — as in a CASE's result branches.
        BoundExpr::Coalesce { args, .. } => combine_all(args),
        BoundExpr::Subscript { base, .. } => expr_collation(base),
        // Literals and parameters carry a string value but assert no collation
        // of their own, so they adapt to whatever they are compared against.
        _ => Derived::NONE,
    }
}

impl Derived {
    /// Keep the stronger of two derivations. On a tie — both sides carrying
    /// the same non-`None` strength but disagreeing on the collation — fall
    /// back to the database default instead of raising the `42P22` PostgreSQL
    /// reports at the point of use, the gap the module header records as a
    /// `TODO`. A genuine *explicit* conflict is caught earlier by
    /// [`check_explicit_conflict`] wherever more than one collatable input
    /// combines, so by the time an explicit/explicit tie reaches here it has
    /// already been rejected; this fallback only actually fires for implicit
    /// ties.
    pub(crate) fn max_with(self, other: Derived) -> Derived {
        if other.strength > self.strength {
            other
        } else if self.strength == other.strength
            && self.strength != Strength::None
            && self.collation != other.collation
        {
            Derived::NONE
        } else {
            self
        }
    }
}

fn combine_all(exprs: &[BoundExpr]) -> Derived {
    exprs
        .iter()
        .map(expr_collation)
        .fold(Derived::NONE, Derived::max_with)
}

/// The collation to record on a rowset column produced by `expr`
/// ([`crate::OutputColumn::collation`]): `Some` only when it differs from what
/// the column's type would give anyway, which `None` already means.
pub fn column_collation(expr: &BoundExpr) -> Option<u32> {
    output_collation(expr).0
}

/// [`column_collation`] paired with the strength behind it, for
/// [`crate::OutputColumn::strength`].
pub fn output_collation(expr: &BoundExpr) -> (Option<u32>, Strength) {
    let derived = expr_collation(expr);
    let collation = (derived.collation != type_collation(expr.ty())).then_some(derived.collation);
    (collation, derived.strength)
}

/// `42P22` when two of `derived` are both explicit and disagree — PostgreSQL's
/// rule for combining more than one collatable input (function arguments,
/// `CASE` branches, `ARRAY` elements, a `UNION`'s arms), generalized from the
/// pairwise check a binary comparison does in [`collation_for_comparison`].
pub fn check_explicit_conflict(
    derived: impl IntoIterator<Item = Derived>,
) -> Result<(), BindError> {
    let mut explicit: Option<u32> = None;
    for d in derived {
        if d.strength != Strength::Explicit {
            continue;
        }
        match explicit {
            Some(prev) if prev != d.collation => return Err(conflict_error(prev, d.collation)),
            _ => explicit = Some(d.collation),
        }
    }
    Ok(())
}

fn conflict_error(a: u32, b: u32) -> BindError {
    BindError::new(
        sqlstate::INDETERMINATE_COLLATION,
        format!(
            "collation mismatch between explicit collations \"{}\" and \"{}\"",
            collation_name(a),
            collation_name(b)
        ),
    )
}

/// The collation a comparison of `left` and `right` should use.
///
/// `42P22` when the two sides carry conflicting *explicit* `COLLATE` clauses,
/// as PostgreSQL does — that is a query the author wrote two ways at once.
pub fn collation_for_comparison(left: &BoundExpr, right: &BoundExpr) -> Result<u32, BindError> {
    let (l, r) = (expr_collation(left), expr_collation(right));
    check_explicit_conflict([l, r])?;
    Ok(l.max_with(r).collation)
}

/// The name of a collation OID, for error text. Falls back to the OID's decimal
/// spelling for an OID with no registry entry, which only a corrupt catalog
/// could produce.
pub fn collation_name(oid: u32) -> String {
    lookup_by_oid(oid).map_or_else(|| oid.to_string(), |def| def.name.to_string())
}

/// Resolve a `COLLATE` name to its OID.
///
/// Collation names are ordinary identifiers: quoted ones keep their case,
/// unquoted ones fold to lower case, which is why `COLLATE "C"` works and
/// `COLLATE C` does not — matching PostgreSQL. A schema qualifier is accepted
/// only for `pg_catalog`, the sole schema holding collations here.
pub fn resolve_collation(name: &ast::ObjectName) -> Result<u32, BindError> {
    let parts: Vec<&ast::Ident> = name.0.iter().filter_map(|p| p.as_ident()).collect();
    let (schema, ident) = match parts.as_slice() {
        [ident] => (None, *ident),
        [schema, ident] => (Some(*schema), *ident),
        _ => return Err(undefined_collation(&name.to_string())),
    };
    if let Some(schema) = schema {
        let schema = ident_text(schema);
        if schema != "pg_catalog" {
            return Err(BindError::new(
                sqlstate::INVALID_SCHEMA_NAME,
                format!("schema \"{schema}\" does not exist"),
            ));
        }
    }
    let spelled = ident_text(ident);
    lookup_by_name(&spelled)
        .map(|def| def.oid)
        .ok_or_else(|| undefined_collation(&spelled))
}

fn undefined_collation(name: &str) -> BindError {
    BindError::new(
        sqlstate::UNDEFINED_OBJECT,
        format!("collation \"{name}\" for encoding \"UTF8\" does not exist"),
    )
}

fn ident_text(ident: &ast::Ident) -> String {
    match ident.quote_style {
        Some(_) => ident.value.clone(),
        None => ident.value.to_lowercase(),
    }
}
