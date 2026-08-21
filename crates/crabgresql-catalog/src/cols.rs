//! The column and datum helpers every catalog module shares: the two catalog-only
//! column types (`"char"`, `regproc`) and the constructors for their values.

use crabgresql_storage_api::Column;
use crabgresql_types::text::quote_ident;
use crabgresql_types::{PgType, Reg, RegKind, Value, VectorKind};

use crate::ProcRef;
use crate::SystemCatalog;
use crate::oids::OWN_AM_HANDLERS;

/// A `"char"` column: PostgreSQL's one-byte ad-hoc type, which is what the
/// catalog's flag columns (`typtype`, `typcategory`, `relkind`, `provolatile`,
/// `castcontext`, …) really are.
pub(crate) const CHARLIKE: PgType = PgType::Char;

/// A `regproc` column: an OID that names a function and prints as that
/// function's name. Distinct from [`CHARLIKE`], which the two shared until the
/// alias was split — `typinput` and friends hold multi-character names a
/// one-byte type would truncate.
pub(crate) const REGPROC: PgType = PgType::Reg(RegKind::Proc);

pub(crate) fn col(name: &str, ty: PgType) -> Column {
    Column::new(name, ty)
}

/// The `rows` half of a relation this deployment has none of.
///
/// Shared by every catalog whose feature does not exist here — triggers, RLS
/// policies, publications, extended statistics, foreign data. Empty is not a
/// placeholder in those: PostgreSQL asked the same question on a database that
/// never used the feature answers with zero rows too, so a client joining one
/// gets the same result from both servers. A relation that later gains rows
/// replaces this with its own builder and one line in [`crate::registry`].
pub(crate) fn no_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    Vec::new()
}

/// A cumulative counter as `bigint`, the type every `pg_stat_*` count has.
///
/// Clamped rather than cast: `as i64` would wrap an unsigned counter to a
/// negative one, and a client subtracting two samples would read that as a
/// reset. Reaching the clamp takes 2^63 rows.
pub(crate) fn counter(n: u64) -> Value {
    Value::Int8(i64::try_from(n).unwrap_or(i64::MAX))
}

/// A `timestamptz` column PostgreSQL leaves NULL until the thing it stamps has
/// happened — `last_vacuum`, `last_seq_scan`, `stats_reset`.
pub(crate) fn stamp_or_null(at: Option<i64>) -> Value {
    match at {
        Some(at) => Value::TimestampTz(at),
        None => Value::Null,
    }
}

/// A `pg_node_tree` column.
///
/// PostgreSQL stores a parsed expression tree here and only ever hands it back
/// through `pg_get_expr`/`pg_get_viewdef`. crabgresql stores the already-deparsed
/// SQL text instead and lets those functions echo it (see
/// [`class::pg_class_schema`](crate::catalogs::class)'s `relpartbound` and
/// `pg_constraint.conbin`), so the column is `text` — the declared type differs,
/// what a client reads through the accessors does not.
pub(crate) const NODE_TREE: PgType = PgType::Text;

/// An `aclitem[]` column: the access-control list PostgreSQL attaches to an
/// object. There is no `GRANT`/`REVOKE` here, so every one of these is NULL —
/// which is also what PostgreSQL reports for an object whose privileges were
/// never changed from the owner's defaults. Typed as `text` for want of an
/// `aclitem` type; nothing renders a value.
pub(crate) const ACLITEM_ARRAY: PgType = PgType::Text;

/// A `"char"` datum from the single character the catalogs spell it with.
pub(crate) fn chr(c: char) -> Value {
    Value::Char(c as u8)
}

/// A `"char"` datum from a string the codegen or a catalog struct carries as
/// text. An empty string becomes `\0`, which is how PostgreSQL stores an unset
/// flag and prints back as the empty string.
pub(crate) fn str_char(s: &str) -> Value {
    Value::Char(s.bytes().next().unwrap_or(0))
}

/// A `regproc` datum from a codegen-resolved reference.
///
/// The name is quoted the way `regprocout` quotes it, which matters for the
/// handful of built-ins named after a keyword (`numeric`, `char`, `time`, …):
/// `pg_cast.castfunc` reads out of this and a `::regproc` cast reads out of
/// `reg.rs`, and the two must not spell one function differently.
pub(crate) fn regproc(r: ProcRef) -> Value {
    // OID 0 is codegen's "no function", and its `-` is a rendering rather than
    // a name — quoting it would print `"-"`.
    if r.oid == 0 {
        return Value::Reg(Reg::unresolved(RegKind::Proc, 0));
    }
    Value::Reg(Reg {
        kind: RegKind::Proc,
        oid: r.oid,
        name: quote_ident(r.name),
    })
}

/// A `regproc` datum for a function named at runtime rather than by codegen —
/// an access method handler, say. An unknown name is `0`, which prints as `-`
/// exactly as PostgreSQL renders a missing reference.
pub(crate) fn regproc_by_name(name: &str) -> Value {
    let own = OWN_AM_HANDLERS
        .iter()
        .find(|(_, handler)| *handler == name)
        .map(|(oid, _)| *oid);
    match own.or_else(|| crate::builtin_proc_oid(name)) {
        Some(oid) => Value::Reg(Reg {
            kind: RegKind::Proc,
            oid,
            name: quote_ident(name),
        }),
        None => Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
    }
}

/// An array column that is NULL rather than empty when there is nothing to
/// report — the shape of `pg_proc.proargnames` and its two neighbours.
/// PostgreSQL stores NULL there, and clients test for NULL, so an empty array
/// would read as "declares zero argument names" instead of "declares none".
pub(crate) fn optional_array(elem: PgType, values: Vec<Value>) -> Value {
    if values.is_empty() {
        Value::Null
    } else {
        Value::Array {
            elem,
            elems: values,
        }
    }
}

/// A `text` datum that is NULL when the generated row carries the empty
/// string — how codegen spells "this column has no value".
pub(crate) fn text_or_null(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        Value::Text(s.to_string())
    }
}

/// A `reg*[]` column of the given kind — `regtype[]` for the type-OID arrays
/// `pg_prepared_statements` reports.
pub(crate) fn reg_array_type(kind: RegKind) -> PgType {
    PgType::Array(kind.oid())
}

/// A `regtype[]` datum from type OIDs, naming each the way `regtype`'s output
/// function does.
///
/// The three tiers match [`crabgresql_executor::reg`]: a built-in prints under
/// its SQL spelling (23 is `integer`), a pseudo-type names itself from the
/// shared table, and a user-defined type comes from the catalog. Only an OID
/// that names nothing left falls back to its digits, which is what PostgreSQL
/// renders for a type it genuinely cannot name.
pub(crate) fn regtype_array(cat: &SystemCatalog, oids: &[u32]) -> Value {
    Value::Array {
        elem: PgType::Reg(RegKind::Type),
        elems: oids.iter().map(|&oid| regtype_named(cat, oid)).collect(),
    }
}

/// One `regtype` datum, named the three-tier way [`regtype_array`] documents.
pub(crate) fn regtype_named(cat: &SystemCatalog, oid: u32) -> Value {
    let name = PgType::from_oid(oid)
        .map(|ty| ty.name().to_string())
        .or_else(|| crabgresql_types::pseudo_type_name(oid).map(str::to_string))
        .or_else(|| {
            cat.user_type_ref(oid)
                .map(|(_, name)| crabgresql_types::text::quote_ident(name))
        });
    Value::Reg(match name {
        Some(name) => Reg {
            kind: RegKind::Type,
            oid,
            name,
        },
        None => Reg::unresolved(RegKind::Type, oid),
    })
}

/// The `oidvector` and `int2vector` catalog column types. See
/// [`crabgresql_types::vector`].
pub(crate) const OIDVECTOR: PgType = PgType::Vector(VectorKind::Oid);
pub(crate) const INT2VECTOR: PgType = PgType::Vector(VectorKind::Int2);

pub(crate) fn oidvector(elems: impl IntoIterator<Item = u32>) -> Value {
    Value::Vector {
        kind: VectorKind::Oid,
        elems: elems.into_iter().map(Value::Oid).collect(),
    }
}

pub(crate) fn int2vector(elems: impl IntoIterator<Item = i16>) -> Value {
    Value::Vector {
        kind: VectorKind::Int2,
        elems: elems.into_iter().map(Value::Int2).collect(),
    }
}

/// Build an [`INT2VECTOR`] of 1-based attribute numbers from 0-based column
/// indexes — the shape of `pg_index.indkey` and
/// `pg_partitioned_table.partattrs`.
///
/// `attnum` is an `int2` in PostgreSQL, which caps a relation at 32767 columns;
/// PostgreSQL never reaches that because it rejects a table past 1600 columns,
/// but this build has no such limit. A column index that does not fit is
/// reported as `0`, which is already PostgreSQL's `indkey` sentinel for "this
/// key is not a plain column reference" — the closest honest rendering. It must
/// not be a bare `as i16`: that panics on overflow in a debug build and wraps to
/// a negative attnum in a release one.
pub(crate) fn attnum_vector(columns: impl IntoIterator<Item = usize>) -> Value {
    int2vector(
        columns
            .into_iter()
            .map(|c| i16::try_from(c.saturating_add(1)).unwrap_or(0)),
    )
}
