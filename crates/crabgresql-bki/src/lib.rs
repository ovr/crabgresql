//! Build-time codegen for `pg_catalog`'s built-in rows.
//!
//! Reads PostgreSQL's vendored catalog *data* files
//! (`vendor/postgres/catalog/*.dat`) and emits the typed row arrays
//! `crabgresql-catalog` includes at compile time. The `.dat` format is a Perl
//! array-of-hashes; [`dat`] reads that DATA only — it is an original scanner,
//! NOT a port of PostgreSQL's `Catalog.pm`. Fields a `.dat` entry omits are
//! filled from the per-catalog defaults in the emitters (authored from the
//! public catalog docs), so codegen never reads PostgreSQL's C headers.
//!
//! This lives in a library rather than in `crabgresql-catalog/build.rs` so the
//! parser, the symbol resolution and the emitters can be unit-tested; the
//! build script is the thin wrapper that calls [`generate`].
//!
//! Codegen runs in the two phases [`symbols`] describes: every catalog defines
//! its symbols, and only then does any catalog resolve a reference. That is
//! what lets `pg_type.typinput` point at a `pg_proc` row whose `prorettype`
//! points back at `pg_type`.
//!
//! See `docs/ARCHITECTURE.md` §7 and `AGENTS.md`: vendoring catalog `.dat` data
//! and generating from it is the sanctioned path; attribution is in `NOTICE`.

pub mod dat;
mod pg_cast;
mod pg_proc;
mod pg_type;
pub mod symbols;

use std::path::Path;

use dat::read_dat;
use symbols::SymbolKind::Proc;
use symbols::SymbolTable;

/// The `pg_am.amhandler` names `crabgresql-catalog`'s `catalogs::am::pg_am_rows`
/// publishes that upstream also has. That catalog is hand-written rather than
/// generated, so its references are declared here instead of being recorded
/// while it is emitted. crabgresql's own access methods (`parquet`, `buffer`)
/// have no upstream entry to resolve against; `oids::OWN_AM_HANDLERS` gives
/// those two `pg_proc` rows of their own.
const AM_HANDLERS: &[&str] = &[
    "heap_tableam_handler",
    "bthandler",
    "hashhandler",
    "gisthandler",
    "ginhandler",
    "brinhandler",
    "spghandler",
];

/// Read the vendored `.dat` files in `catalog_dir` and write the generated row
/// arrays into `out_dir` (cargo's `OUT_DIR`).
///
/// `pg_proc` is emitted last on purpose: it emits exactly the functions the
/// other catalogs turned out to reference, and [`SymbolTable::references`]
/// enforces that ordering.
pub fn generate(catalog_dir: &Path, out_dir: &Path) -> std::io::Result<()> {
    let type_entries = read_dat(catalog_dir, "pg_type.dat")?;
    let cast_entries = read_dat(catalog_dir, "pg_cast.dat")?;
    let proc_entries = read_dat(catalog_dir, "pg_proc.dat")?;

    // Phase one: define.
    let mut symbols = SymbolTable::default();
    pg_type::define_symbols(&type_entries, &mut symbols);
    pg_proc::define_symbols(&proc_entries, &mut symbols);

    // Phase two: resolve and emit.
    std::fs::write(
        out_dir.join("pg_type_rows.rs"),
        pg_type::emit(&type_entries, &symbols),
    )?;
    std::fs::write(
        out_dir.join("pg_cast_rows.rs"),
        pg_cast::emit(&cast_entries, &symbols),
    )?;
    for handler in AM_HANDLERS {
        symbols.resolve_name(Proc, handler);
    }
    std::fs::write(
        out_dir.join("pg_proc_rows.rs"),
        pg_proc::emit(&proc_entries, &symbols),
    )?;
    Ok(())
}

/// A `regproc` column as the `ProcRef` expression the generated file carries:
/// the name as written, plus the OID it resolves to. `-` is the catalog's
/// spelling of "no function" and resolves to 0, which prints back as `-`.
fn proc_ref(symbols: &SymbolTable, name: &str) -> String {
    let oid = symbols.resolve_name(Proc, name).unwrap_or(0);
    format!("ProcRef {{ oid: {oid}, name: {name:?} }}")
}
