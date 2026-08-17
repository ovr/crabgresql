//! `pg_proc` publishes exactly the function names this build resolves.
//!
//! Codegen filters `pg_proc.dat` by two justifications (see
//! `crabgresql-bki`'s `pg_proc` module): an inbound reference from another
//! catalog, or `crabgresql-bki`'s `IMPLEMENTED_PRONAMES` — the SQL surface
//! nothing in the vendored data points at. That manifest duplicates what the
//! binder's registry already knows, and this is where the duplication is
//! checked instead of trusted: the two crates cannot see each other (the
//! catalog is below the binder), but this one sees both.
//!
//! Why both directions matter:
//!
//!   - a function added to the binder without a manifest entry is invisible to
//!     introspection — `'newfunc'::regproc` fails, and a client cannot find it
//!     in `pg_proc` at all;
//!   - a manifest entry the binder cannot resolve is a `pg_proc` row that lies:
//!     the row says the function exists and calling it raises `42883`.
//!
//! The universe walked here is every `proname` in `pg_proc.dat`, not the
//! binder's registry, because a registry of `match` arms cannot be enumerated
//! from outside. Asking upstream's whole name list about each name covers every
//! function this build implements that PostgreSQL also has — which is all of
//! them, since a `pg_proc` row can only carry an upstream OID.

use std::path::PathBuf;

use crabgresql_bki::dat::{Entry, get, parse_dat};
use crabgresql_bki::implemented::IMPLEMENTED_PRONAMES;
use crabgresql_catalog::PG_PROC_ROWS;

/// Every `proname` `pg_proc.dat` defines, deduplicated.
fn upstream_pronames() -> Vec<String> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/postgres/catalog/pg_proc.dat");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let entries: Vec<Entry> = parse_dat(&text);
    let mut names: Vec<String> = entries
        .iter()
        .filter_map(|e| get(e, "proname").map(str::to_string))
        .collect();
    names.sort();
    names.dedup();
    assert!(
        names.len() > 2000,
        "pg_proc.dat yielded only {} names, which is not the whole file",
        names.len()
    );
    names
}

fn publishes(name: &str) -> bool {
    PG_PROC_ROWS.iter().any(|row| row.proname == name)
}

/// A function the binder resolves has a `pg_proc` row, so a client can find it.
#[test]
fn every_implemented_function_is_published() {
    let missing: Vec<String> = upstream_pronames()
        .into_iter()
        .filter(|name| crabgresql_binder::implements_function(name) && !publishes(name))
        .collect();
    assert!(
        missing.is_empty(),
        "these functions resolve but pg_proc publishes no row for them — add each \
         to crabgresql-bki's IMPLEMENTED_PRONAMES: {missing:?}"
    );
}

/// And the reverse: nothing in the manifest claims a function the binder cannot
/// resolve. A row that outlives its implementation is worse than a missing one,
/// since introspection finds it and the call then raises `42883`.
#[test]
fn every_manifest_name_resolves() {
    let unimplemented: Vec<&str> = IMPLEMENTED_PRONAMES
        .iter()
        .copied()
        .filter(|name| !crabgresql_binder::implements_function(name))
        .collect();
    assert!(
        unimplemented.is_empty(),
        "IMPLEMENTED_PRONAMES claims functions the binder does not resolve: {unimplemented:?}"
    );
    // And each is actually published — the manifest is a filter, so a name that
    // reached it and no row would mean codegen dropped it.
    for name in IMPLEMENTED_PRONAMES {
        assert!(
            publishes(name),
            "the manifest lists {name}, pg_proc has no row"
        );
    }
}

/// The point of the whole exercise, spot-checked at the surface a client reads:
/// the everyday functions are there, under upstream's OIDs, and a function this
/// build does not implement is not.
#[test]
fn the_published_surface_is_upstreams_own_rows() {
    let oid_of = |name: &str, nargs: i16| {
        PG_PROC_ROWS
            .iter()
            .find(|row| row.proname == name && row.pronargs == nargs)
            .unwrap_or_else(|| panic!("pg_proc publishes no {name} of {nargs} argument(s)"))
            .oid
    };
    // Probed against PostgreSQL 18.4: `'upper'::regproc::oid` and friends.
    assert_eq!(oid_of("upper", 1), 871);
    assert_eq!(oid_of("now", 0), 1299);
    assert_eq!(oid_of("md5", 1), 2311);
    assert_eq!(oid_of("row_number", 0), 3100);
    // `make_interval`'s seven arguments all carry defaults, which is a count
    // this build derives rather than a set of expressions it stores.
    let make_interval = PG_PROC_ROWS
        .iter()
        .find(|row| row.proname == "make_interval")
        .expect("make_interval is published");
    assert_eq!(
        (make_interval.pronargs, make_interval.pronargdefaults),
        (7, 7)
    );
    // A window function keeps upstream's `prokind`, so `\df` sorts it as one.
    assert_eq!(
        PG_PROC_ROWS
            .iter()
            .find(|row| row.proname == "row_number")
            .map(|row| row.prokind),
        Some("w")
    );
    // Nothing here implements XML, and no catalog references its functions.
    assert!(!publishes("xmlcomment"));
}
