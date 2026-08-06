//! Build-time codegen for `pg_catalog`'s built-in rows.
//!
//! The work — the `.dat` scanner, the two-phase symbol resolver and the
//! per-catalog emitters — lives in `crabgresql-bki`, where it can be
//! unit-tested. This script only says where to read and where to write.

use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let catalog_dir = manifest.join("../../vendor/postgres/catalog");
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    crabgresql_bki::generate(&catalog_dir, &out_dir)?;
    Ok(())
}
