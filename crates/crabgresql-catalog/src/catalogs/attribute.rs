//! `pg_attribute` and `pg_attrdef`: the columns and their defaults.

use crabgresql_storage_api::{SysCol, TableSchema};
use crabgresql_types::{PgType, Value};

use crate::cols::*;
use crate::{RelKind, SystemCatalog};
use crabgresql_storage_api::Column;

use crate::PG_TYPE_ROWS;
use crate::catalogs::class::TOAST_COLUMNS;
use crate::catalogs::collation::typcollation_of;

/// `pg_catalog.pg_attribute` — a curated subset of columns for user relations'
/// columns.
pub(crate) fn pg_attribute_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_attribute",
        "pg_catalog",
        vec![
            col("attrelid", PgType::Oid),
            col("attname", PgType::Name),
            col("atttypid", PgType::Oid),
            col("attlen", PgType::Int2),
            col("attnum", PgType::Int2),
            col("atttypmod", PgType::Int4),
            col("attndims", PgType::Int2),
            col("attbyval", PgType::Bool),
            col("attalign", CHARLIKE),
            col("attstorage", CHARLIKE),
            col("attnotnull", PgType::Bool),
            col("atthasdef", PgType::Bool),
            col("attidentity", CHARLIKE),
            col("attgenerated", CHARLIKE),
            col("attisdropped", PgType::Bool),
            // TODO: `Column` carries no inheritance provenance — unlike
            // `CheckConstraint`, which already has `islocal`/`inhcount` — so
            // every row claims the column was declared on the relation. A
            // client reading `not attislocal` (DataGrip's column introspection
            // does) shows an inherited column as declared here.
            col("attislocal", PgType::Bool),
            col("attinhcount", PgType::Int2),
            col("attcollation", PgType::Oid),
            // aclitem[]; represented as text and always NULL (default ACL) here.
            // NULL is the whole truth rather than a placeholder: there is no
            // `GRANT` in this build, so no column ever carries an ACL. Same
            // treatment as `pg_namespace.nspacl`.
            col("attacl", PgType::Text),
            // text[]; only a foreign table's column ever carries one, and there
            // are no foreign tables here, so every row is NULL — as it is
            // upstream for every column of an ordinary relation.
            col("attfdwoptions", PgType::Array(crabgresql_types::oid::TEXT)),
        ],
    )
}

/// A column's physical layout — `attbyval`, `attalign`, `attstorage` — taken
/// from the *type's* `pg_type` row rather than restated here, which is what
/// upstream's `type_sanity` checks the two agree on. A type with no built-in
/// row (a `CREATE TYPE` enum) reports the fixed 4-byte pass-by-value layout
/// [`crate::catalogs::types::pg_type_user_rows`] gives it.
fn attlayout_of(ty: PgType) -> (Value, Value, Value) {
    match PG_TYPE_ROWS.iter().find(|row| row.oid == ty.oid()) {
        Some(row) => (
            Value::Bool(row.typbyval),
            str_char(row.typalign),
            str_char(row.typstorage),
        ),
        None => (Value::Bool(true), chr('i'), chr('p')),
    }
}

/// `attndims`: how many array dimensions the column was *declared* with.
/// PostgreSQL records the declaration and never enforces it — a value of any
/// dimensionality fits any array column. This build's arrays are one-dimensional
/// throughout.
fn attndims_of(ty: PgType) -> Value {
    Value::Int2(match ty {
        PgType::Array(_) => 1,
        _ => 0,
    })
}

/// `attcollation`: the column's explicit `COLLATE`, else the type's own
/// collation — and `0` when the type has none, as PostgreSQL records it.
pub(crate) fn attcollation_of(column: &Column) -> u32 {
    match column.collation {
        Some(oid) => oid,
        None => typcollation_of(column.ty.oid()),
    }
}

/// The six system-attribute rows for the relation `oid` names.
/// Taken from [`SysCol`] rather than restated: the binder's list is what a query
/// actually resolves against, and a `pg_attribute` row exists to make a client's
/// column list agree with what the server will answer. A second copy here could
/// drift into advertising a column the server refuses, or omitting one it serves.
fn system_attribute_rows(oid: u32) -> Vec<Vec<Value>> {
    SysCol::ALL
        .iter()
        .map(|col| {
            let ty = &col.ty();
            let (byval, align, storage) = attlayout_of(*ty);
            vec![
                Value::Oid(oid),
                Value::Text(col.name().to_string()),
                Value::Oid(ty.oid()),
                Value::Int2(ty.typlen()),
                Value::Int2(col.attnum()),
                Value::Int4(-1),
                attndims_of(*ty),
                byval,
                align,
                storage,
                // PostgreSQL marks every system attribute NOT NULL, even the
                // ones an outer join reports as NULL: `attnotnull` describes the
                // stored column, not what a query can produce from it.
                Value::Bool(true),
                Value::Bool(false),
                chr('\0'),
                chr('\0'),
                Value::Bool(false),
                Value::Bool(true),
                Value::Int2(0),
                Value::Oid(0),
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}

/// Build `pg_attribute` rows: one per column of each relation, `attnum` 1-based
/// (user columns only) plus the six system attributes at negative `attnum`,
/// typed from the column's `PgType` (`atttypid`/`attlen`).
pub(crate) fn pg_attribute_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let (relations, indexes, toasts) = (cat.relation_oids(), cat.index_oids(), cat.toast_oids());
    let mut rows = Vec::new();
    for ((oid, schema), kind) in relations.iter().zip(cat.relation_kinds()) {
        // A view has no system attributes upstream, because it has no rows of
        // its own for one to describe — the same reason the binder exposes no
        // system column on one. Probed against PostgreSQL 18.4: `r`/`p`/`S`/`m`
        // and a TOAST table publish all six, `v` and `i` publish none.
        if *kind != RelKind::View {
            rows.extend(system_attribute_rows(*oid));
        }
        for (i, c) in schema.columns.iter().enumerate() {
            let (byval, align, storage) = attlayout_of(c.ty);
            rows.push(vec![
                Value::Oid(*oid),
                Value::Text(c.name.clone()),
                Value::Oid(c.ty.oid()),
                Value::Int2(c.ty.typlen()),
                Value::Int2((i + 1) as i16),
                Value::Int4(c.atttypmod()),
                attndims_of(c.ty),
                byval,
                align,
                storage,
                Value::Bool(!c.nullable),
                // `atthasdef` covers a generation expression too: upstream keeps
                // it in `pg_attrdef` and flags the column here, which is what
                // makes psql's `\d` join find it.
                Value::Bool(c.default.is_some() || c.generated.is_some()),
                // attidentity: identity columns do not exist in this build.
                // PostgreSQL spells "not one" as `\0`, which prints empty.
                chr('\0'),
                match &c.generated {
                    Some(g) => chr(g.kind.attgenerated()),
                    None => chr('\0'),
                },
                Value::Bool(false),
                Value::Bool(true),
                Value::Int2(0),
                Value::Oid(attcollation_of(c)),
                Value::Null,
                Value::Null,
            ]);
        }
    }
    for index in indexes {
        for (position, key) in index.metadata.keys.iter().enumerate() {
            let column = &index.table_schema.columns[key.column];
            let (byval, align, storage) = attlayout_of(column.ty);
            rows.push(vec![
                Value::Oid(index.oid),
                Value::Text(column.name.clone()),
                Value::Oid(column.ty.oid()),
                Value::Int2(column.ty.typlen()),
                Value::Int2((position + 1) as i16),
                Value::Int4(column.atttypmod()),
                attndims_of(column.ty),
                byval,
                align,
                storage,
                Value::Bool(false),
                Value::Bool(false),
                chr('\0'),
                chr('\0'),
                Value::Bool(false),
                Value::Bool(true),
                Value::Int2(0),
                Value::Oid(attcollation_of(column)),
                Value::Null,
                Value::Null,
            ]);
        }
    }
    // A TOAST relation's columns, so its `pg_class.relnatts` has rows to join.
    // It is an ordinary heap upstream, so it carries the system attributes too.
    for toast in toasts {
        rows.extend(system_attribute_rows(toast.oid));
        for (i, (name, ty)) in TOAST_COLUMNS.iter().enumerate() {
            let (byval, align, storage) = attlayout_of(*ty);
            rows.push(vec![
                Value::Oid(toast.oid),
                Value::Text((*name).to_string()),
                Value::Oid(ty.oid()),
                Value::Int2(ty.typlen()),
                Value::Int2((i + 1) as i16),
                Value::Int4(-1),
                attndims_of(*ty),
                byval,
                align,
                storage,
                // PostgreSQL marks all three NOT NULL.
                Value::Bool(true),
                Value::Bool(false),
                chr('\0'),
                chr('\0'),
                Value::Bool(false),
                Value::Bool(true),
                Value::Int2(0),
                Value::Oid(0),
                Value::Null,
                Value::Null,
            ]);
        }
    }
    rows
}

pub(crate) fn pg_attrdef_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_attrdef",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("adrelid", PgType::Oid),
            col("adnum", PgType::Int2),
            col("adbin", PgType::Text),
        ],
    )
}

/// Render the defaults [`crate::SystemCatalog::attrdef_oids`] already numbered.
/// Pure, for the reason [`crate::catalogs::constraint::pg_constraint_rows`] is:
/// the OID a row prints is the one `pg_depend` names as the dependent object.
///
/// A generated column's expression is a row here too — `pg_get_expr(adbin,
/// adrelid)` reads both back, and `attgenerated` is what tells them apart.
pub(crate) fn pg_attrdef_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    cat.attrdef_oids()
        .iter()
        .map(|d| {
            vec![
                Value::Oid(d.oid),
                Value::Oid(d.table_oid),
                Value::Int2(d.attnum),
                Value::Text(d.expr.clone()),
            ]
        })
        .collect()
}
