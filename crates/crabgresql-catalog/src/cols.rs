//! The column and datum helpers every catalog module shares: the two catalog-only
//! column types (`"char"`, `regproc`) and the constructors for their values.

use crabgresql_storage_api::Column;
use crabgresql_types::{PgType, Reg, RegKind, Value, VectorKind};

use crate::ProcRef;
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
pub(crate) fn regproc(r: ProcRef) -> Value {
    Value::Reg(Reg {
        kind: RegKind::Proc,
        oid: r.oid,
        name: r.name.to_string(),
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
            name: name.to_string(),
        }),
        None => Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
    }
}

/// The `oidvector` and `int2vector` catalog column types. See
/// [`crabgresql_types::vector`].
pub(crate) const OIDVECTOR: PgType = PgType::Vector(VectorKind::Oid);
pub(crate) const INT2VECTOR: PgType = PgType::Vector(VectorKind::Int2);

/// Build an [`OIDVECTOR`] value from a sequence of OIDs.
pub(crate) fn oidvector(elems: impl IntoIterator<Item = u32>) -> Value {
    Value::Vector {
        kind: VectorKind::Oid,
        elems: elems.into_iter().map(Value::Oid).collect(),
    }
}

/// Build an [`INT2VECTOR`] value from a sequence of `int2`s.
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
