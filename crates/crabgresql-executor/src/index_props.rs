//! The index access-method property tables behind `pg_indexam_has_property`,
//! `pg_index_has_property` and `pg_index_column_has_property`.
//!
//! Clean-room (see AGENTS.md): every answer below is transcribed from the
//! *output* of those three functions on PostgreSQL 18.4 — `amutils.sql`'s full
//! matrix, cross-checked against a live server — not from the access methods'
//! C flag bits.
//!
//! Three levels, and a property belongs to exactly one of them: asking a
//! level about a property it does not own yields NULL rather than false. So
//! `pg_indexam_has_property(403, 'asc')` is NULL even though `asc` is a real
//! property name, and every unrecognized name is NULL at all three levels.

use crabgresql_storage_api::{IndexKey, IndexMethod};

/// OIDs of PostgreSQL's index access methods, as `pg_am` publishes them — the
/// same numbers `crabgresql-catalog`'s `BUILTIN_AMS` carries, restated because
/// the executor does not depend on the catalog crate.
///
/// Index AMs only. A *table* AM's OID (`heap`, or crabgresql's own `parquet` and
/// `buffer`) answers NULL for every property on PostgreSQL, which is what falling
/// off the end of this table already gives; and with no `CREATE ACCESS METHOD`
/// nothing can join `pg_am` at run time either.
const BTREE: u32 = 403;
const HASH: u32 = 405;
const GIST: u32 = 783;
const GIN: u32 = 2742;
const BRIN: u32 = 3580;
const SPGIST: u32 = 4000;

/// The five AM-level properties in the order
/// `(can_order, can_unique, can_multi_col, can_exclude, can_include)`.
const AM_PROPERTIES: &[(u32, [bool; 5])] = &[
    (BTREE, [true, true, true, true, true]),
    (HASH, [false, false, false, true, false]),
    (GIST, [false, false, true, true, true]),
    (GIN, [false, false, true, false, false]),
    (BRIN, [false, false, true, false, false]),
    (SPGIST, [false, false, false, true, true]),
];

/// `pg_indexam_has_property(am, prop)`. NULL (`None`) for a property that is not
/// AM-level and for an OID that is not an index access method — neither is an
/// error in PostgreSQL.
pub(crate) fn indexam_property(am_oid: u32, prop: &str) -> Option<bool> {
    let index = [
        "can_order",
        "can_unique",
        "can_multi_col",
        "can_exclude",
        "can_include",
    ]
    .iter()
    .position(|name| prop.eq_ignore_ascii_case(name))?;
    let (_, props) = AM_PROPERTIES.iter().find(|(oid, _)| *oid == am_oid)?;
    Some(props[index])
}

/// `pg_index_has_property(index, prop)`: the four whole-index properties, which
/// depend only on the index's access method.
///
/// [`IndexMethod`] has just the two variants because those are the only indexes
/// this build can create, so the match is exhaustive rather than a lookup: a
/// `gist` index cannot be reached through this function at all.
pub(crate) fn index_property(method: IndexMethod, prop: &str) -> Option<bool> {
    let props = match method {
        IndexMethod::BTree => [true, true, true, true],
        IndexMethod::Hash => [false, true, true, true],
    };
    let index = ["clusterable", "index_scan", "bitmap_scan", "backward_scan"]
        .iter()
        .position(|name| prop.eq_ignore_ascii_case(name))?;
    Some(props[index])
}

/// `pg_index_column_has_property(index, column, prop)` for one key column.
///
/// A btree column answers its own sort options for the four ordering properties;
/// hash orders nothing, so it answers `false` — not NULL — to all nine. (PG
/// reports NULL for the ordering four only on a *non-key* `INCLUDE` column, which
/// [`crabgresql_storage_api::IndexMetadata`] cannot represent: DDL rejects
/// `INCLUDE` rather than store it.)
pub(crate) fn index_column_property(
    method: IndexMethod,
    key: &IndexKey,
    prop: &str,
) -> Option<bool> {
    let orderable = matches!(method, IndexMethod::BTree);
    Some(match prop.to_ascii_lowercase().as_str() {
        "orderable" => orderable,
        "asc" => orderable && !key.descending,
        "desc" => orderable && key.descending,
        "nulls_first" => orderable && key.nulls_first,
        "nulls_last" => orderable && !key.nulls_first,
        // No access method here offers ordered-by-distance scans; btree returns
        // its index tuples and searches arrays and NULLs, hash does neither.
        "distance_orderable" => false,
        "returnable" | "search_array" | "search_nulls" => orderable,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `amtype = 'i'` matrix of PostgreSQL 18.4's `amutils` test, verbatim.
    #[test]
    fn am_matrix_matches_postgres() {
        let expected: &[(u32, &str, [bool; 5])] = &[
            (BRIN, "brin", [false, false, true, false, false]),
            (BTREE, "btree", [true, true, true, true, true]),
            (GIN, "gin", [false, false, true, false, false]),
            (GIST, "gist", [false, false, true, true, true]),
            (HASH, "hash", [false, false, false, true, false]),
            (SPGIST, "spgist", [false, false, false, true, true]),
        ];
        let props = [
            "can_order",
            "can_unique",
            "can_multi_col",
            "can_exclude",
            "can_include",
        ];
        for (oid, amname, want) in expected {
            for (prop, want) in props.iter().zip(want) {
                assert_eq!(indexam_property(*oid, prop), Some(*want), "{amname}.{prop}");
            }
            assert_eq!(indexam_property(*oid, "bogus"), None, "{amname}.bogus");
            // Index- and column-level property names are not AM-level ones.
            assert_eq!(indexam_property(*oid, "index_scan"), None, "{amname}");
            assert_eq!(indexam_property(*oid, "asc"), None, "{amname}");
        }
        // Case-insensitive, as PG's `pg_strcasecmp` lookup is.
        assert_eq!(indexam_property(BTREE, "CAN_ORDER"), Some(true));
        // `heap` (2) is a table AM, 16_000/16_001 are crabgresql's own, and
        // nothing answers to 999_999.
        for oid in [0, 2, 16_000, 16_001, 999_999] {
            assert_eq!(indexam_property(oid, "can_order"), None, "oid {oid}");
        }
    }

    #[test]
    fn index_level_properties() {
        for (prop, btree, hash) in [
            ("clusterable", true, false),
            ("index_scan", true, true),
            ("bitmap_scan", true, true),
            ("backward_scan", true, true),
        ] {
            assert_eq!(
                index_property(IndexMethod::BTree, prop),
                Some(btree),
                "{prop}"
            );
            assert_eq!(
                index_property(IndexMethod::Hash, prop),
                Some(hash),
                "{prop}"
            );
        }
        assert_eq!(index_property(IndexMethod::BTree, "bogus"), None);
        // AM-level and column-level names are not whole-index ones.
        assert_eq!(index_property(IndexMethod::BTree, "can_order"), None);
        assert_eq!(index_property(IndexMethod::BTree, "asc"), None);
    }

    #[test]
    fn column_level_properties() {
        let key = |descending, nulls_first| IndexKey {
            column: 0,
            descending,
            nulls_first,
        };
        // `a DESC` implies NULLS FIRST, `a` implies NULLS LAST.
        let desc = key(true, true);
        let asc = key(false, false);
        let btree = |k: &IndexKey, prop| index_column_property(IndexMethod::BTree, k, prop);
        assert_eq!(btree(&desc, "asc"), Some(false));
        assert_eq!(btree(&desc, "desc"), Some(true));
        assert_eq!(btree(&desc, "nulls_first"), Some(true));
        assert_eq!(btree(&desc, "nulls_last"), Some(false));
        assert_eq!(btree(&asc, "asc"), Some(true));
        assert_eq!(btree(&asc, "nulls_last"), Some(true));
        for prop in ["orderable", "returnable", "search_array", "search_nulls"] {
            assert_eq!(btree(&asc, prop), Some(true), "{prop}");
        }
        assert_eq!(btree(&asc, "distance_orderable"), Some(false));
        assert_eq!(btree(&asc, "bogus"), None);
        // Hash answers false — not NULL — to every one of the nine.
        for prop in [
            "asc",
            "desc",
            "nulls_first",
            "nulls_last",
            "orderable",
            "distance_orderable",
            "returnable",
            "search_array",
            "search_nulls",
        ] {
            assert_eq!(
                index_column_property(IndexMethod::Hash, &asc, prop),
                Some(false),
                "{prop}"
            );
        }
        assert_eq!(
            index_column_property(IndexMethod::Hash, &asc, "bogus"),
            None
        );
    }
}
