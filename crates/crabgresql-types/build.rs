//! Captures the build environment the `version()` function reports.
//!
//! The target triple and the compiler's own version are known to Cargo and to
//! `rustc`, not to the compiled code, so they are baked in here as environment
//! variables rather than re-derived at run time from `std::env::consts`, whose
//! `ARCH`/`OS` pair is not a triple.

use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo::rustc-env=CRABGRESQL_TARGET={target}");

    // `rustc --version` prints `rustc 1.90.0 (hash date)`; PostgreSQL's
    // `version()` names the compiler and its version, not its build hash, so
    // the parenthetical is dropped.
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.split('(').next().unwrap_or_default().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "rustc".to_string());
    println!("cargo::rustc-env=CRABGRESQL_RUSTC_VERSION={version}");
}
