//! The server's version identity: one place for the string every surface that
//! announces a version reads from.
//!
//! Three surfaces report it and clients cross-check them: the `server_version`
//! GUC (also sent in the startup packet, which is how drivers decide which
//! protocol features to use), the `server_version_num` GUC (what psql shapes
//! its catalog queries by), and the `version()` function. They must agree, so
//! they are derived from the constants here rather than written out separately.

/// The `server_version` string: the PostgreSQL version CrabgreSQL reports
/// parity with, followed by our own crate version.
///
/// PostgreSQL puts the distribution's identity in parentheses after the version
/// the same way (`17.2 (Debian 17.2-1)`), and clients that parse this parse the
/// leading version and stop, so the suffix is safe to carry.
pub const SERVER_VERSION: &str = concat!("19.0 (CrabgreSQL ", env!("CARGO_PKG_VERSION"), ")");

/// The `server_version_num` string: `major * 10000 + minor`, PG's own encoding.
///
/// This is the form clients compare numerically, so it must track
/// [`SERVER_VERSION`]'s leading version.
pub const SERVER_VERSION_NUM: &str = "190000";

/// The target triple this server was built for, e.g. `aarch64-apple-darwin`.
pub const TARGET: &str = env!("CRABGRESQL_TARGET");

/// The compiler that built this server, e.g. `rustc 1.90.0`.
pub const COMPILER: &str = env!("CRABGRESQL_RUSTC_VERSION");

/// What `version()` answers.
///
/// PostgreSQL's shape is `PostgreSQL <version> on <target>, compiled by
/// <compiler>, <bits>-bit`. Tests in the wild match on that shape — the
/// upstream regress suite itself checks `version() ~ 'powerpc64[^,]*-linux-gnu'`
/// — so the field order and the comma placement are part of the contract, not
/// cosmetic.
pub fn version_string() -> String {
    format!(
        "PostgreSQL {SERVER_VERSION} on {TARGET}, compiled by {COMPILER}, {}-bit",
        usize::BITS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_has_pg_shape() {
        let v = version_string();
        assert!(v.starts_with("PostgreSQL 19.0 (CrabgreSQL "), "{v}");
        assert!(v.ends_with("-bit"), "{v}");
        // Target, compiler and word size are three comma-separated fields after
        // the version, which is what a client regexp of the `on <triple>,` form
        // relies on.
        assert_eq!(v.matches(", ").count(), 2, "{v}");
        assert!(v.contains(" on "), "{v}");
    }

    /// The two GUCs are read as one fact by clients; a bump to one that misses
    /// the other silently misroutes version-gated queries.
    #[test]
    fn version_num_tracks_server_version() {
        let major: u32 = SERVER_VERSION
            .split('.')
            .next()
            .and_then(|s| s.parse().ok())
            .expect("leading major version");
        let minor: u32 = SERVER_VERSION
            .split('.')
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .and_then(|s| s.parse().ok())
            .expect("minor version");
        assert_eq!(
            SERVER_VERSION_NUM,
            (major * 10000 + minor).to_string(),
            "server_version_num must encode {SERVER_VERSION}"
        );
    }
}
