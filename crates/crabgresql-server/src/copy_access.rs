//! Which files a server-side `COPY <table> FROM '<file>'` may read.
//!
//! PostgreSQL restricts this form to superusers (or `pg_read_server_files`)
//! because it reads with the server process's privileges, not the client's. This
//! project has no role system yet, so authorization is by **path** instead: a
//! read must land inside the data directory or one of the roots the operator
//! configured, and everything else is refused before the file is touched.
//!
//! A relative path resolves against the data directory, as it does in PG, where
//! the backend's working directory *is* PGDATA.

use std::path::{Component, Path, PathBuf};

use crabgresql_pg_wire::sqlstate;

use crate::error::PgError;

/// The directories a server-side COPY may read from.
#[derive(Clone, Debug)]
pub struct CopyFileAccess {
    /// Where a relative path resolves, and always a permitted root. `None` for a
    /// server with no data directory (the in-memory entry point), which then
    /// permits nothing.
    data_dir: Option<PathBuf>,
    /// Additional permitted roots, already normalized.
    allowed_roots: Vec<PathBuf>,
}

impl CopyFileAccess {
    /// Reads confined to `data_dir`, which is also where a relative path
    /// resolves.
    pub fn confined_to(data_dir: &Path) -> Self {
        CopyFileAccess {
            data_dir: Some(normalize(data_dir)),
            allowed_roots: Vec::new(),
        }
    }

    /// Permit `root` as well. Used by harnesses whose fixture data lives outside
    /// the throwaway data directory, and by the operator's `--copy-allow-path`.
    pub fn allowing(mut self, root: &Path) -> Self {
        self.allowed_roots.push(normalize(root));
        self
    }

    /// Permit nothing: every server-side COPY read is refused. The default for a
    /// server that has no data directory to anchor a policy on.
    pub fn deny_all() -> Self {
        CopyFileAccess {
            data_dir: None,
            allowed_roots: Vec::new(),
        }
    }

    /// Resolve the path a statement named into one this server will open, or
    /// refuse it.
    ///
    /// Confinement is decided *before* the target is touched, so a path outside
    /// every root gets the same answer whether or not it exists — otherwise the
    /// error itself would be a probe for files the client may not read.
    pub fn resolve_for_read(&self, raw: &str) -> Result<PathBuf, PgError> {
        let requested = Path::new(raw);
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            match &self.data_dir {
                // PG resolves a relative COPY path against the data directory.
                Some(data_dir) => data_dir.join(requested),
                None => return Err(denied(requested.is_absolute())),
            }
        };
        let resolved = normalize(&joined);
        if !self.permits(&resolved) {
            return Err(denied(requested.is_absolute()));
        }
        Ok(resolved)
    }

    /// Whether an already-normalized path lies under a permitted root.
    pub fn permits(&self, resolved: &Path) -> bool {
        self.data_dir
            .iter()
            .chain(self.allowed_roots.iter())
            .any(|root| resolved.starts_with(root))
    }

    /// The refusal for `raw`, for a caller that discovered out-of-bounds later
    /// than [`Self::resolve_for_read`] could — a symlink swapped after the
    /// check. Same error either way, so the timing is not observable.
    pub fn denial(&self, raw: &str) -> PgError {
        denied(Path::new(raw).is_absolute())
    }
}

/// PG's `genfile.c` wording for a path outside what the server will read. The
/// two spellings mirror how PG distinguishes the two ways to escape.
fn denied(was_absolute: bool) -> PgError {
    let message = if was_absolute {
        "absolute path not allowed"
    } else {
        "path must be in or below the current directory"
    };
    PgError::new(sqlstate::INSUFFICIENT_PRIVILEGE, message)
}

/// Resolve a path as far as it exists, then fold the rest lexically.
///
/// Plain `canonicalize` is not enough on its own: it fails outright on a file
/// that does not exist, and a missing file must still reach the reader so it can
/// report PG's "could not open file" rather than a confinement error. Resolving
/// the deepest existing ancestor is also what makes containment hold through a
/// symlinked root — on macOS a temp directory is `/var/folders/…` but resolves
/// to `/private/var/folders/…`, so comparing unresolved paths would reject every
/// path under it.
fn normalize(path: &Path) -> PathBuf {
    // Fold `.` and `..` first so a `..` cannot walk out of a root lexically, and
    // so the ancestor search does not stat paths the caller never named.
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                folded.pop();
            }
            other => folded.push(other),
        }
    }

    // The deepest existing ancestor resolves symlinks; the remainder is appended
    // as written.
    let mut suffix: Vec<&std::ffi::OsStr> = Vec::new();
    let mut probe: &Path = &folded;
    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut out = real;
            for part in suffix.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (probe.file_name(), probe.parent()) {
            (Some(name), Some(parent)) => {
                suffix.push(name);
                probe = parent;
            }
            _ => return folded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_resolve_against_the_data_dir() -> Result<(), PgError> {
        let dir = tempfile::tempdir().expect("temp dir");
        let access = CopyFileAccess::confined_to(dir.path());
        let resolved = access.resolve_for_read("fixtures/rows.data")?;
        assert_eq!(resolved, normalize(dir.path()).join("fixtures/rows.data"));
        Ok(())
    }

    #[test]
    fn a_missing_file_inside_a_root_still_resolves() -> Result<(), PgError> {
        // Confinement must not depend on the target existing: the reader is what
        // reports "could not open file", with PG's wording.
        let dir = tempfile::tempdir().expect("temp dir");
        let access = CopyFileAccess::confined_to(dir.path());
        let resolved = access.resolve_for_read("nope.data")?;
        assert!(resolved.starts_with(normalize(dir.path())));
        Ok(())
    }

    #[test]
    fn absolute_paths_outside_every_root_are_denied() {
        let dir = tempfile::tempdir().expect("temp dir");
        let access = CopyFileAccess::confined_to(dir.path());
        let err = access
            .resolve_for_read("/etc/passwd")
            .expect_err("a path outside the data dir must be refused");
        assert_eq!(err.code, sqlstate::INSUFFICIENT_PRIVILEGE);
        assert_eq!(err.message, "absolute path not allowed");
    }

    #[test]
    fn a_parent_escape_cannot_leave_the_data_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let access = CopyFileAccess::confined_to(dir.path());
        let err = access
            .resolve_for_read("../../etc/passwd")
            .expect_err("`..` must not escape the data dir");
        assert_eq!(err.code, sqlstate::INSUFFICIENT_PRIVILEGE);
        assert_eq!(
            err.message,
            "path must be in or below the current directory"
        );
    }

    #[test]
    fn a_denial_does_not_reveal_whether_the_target_exists() {
        let dir = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let present = outside.path().join("present.data");
        std::fs::write(&present, b"1\n").expect("write fixture");
        let access = CopyFileAccess::confined_to(dir.path());

        let existing = access
            .resolve_for_read(&present.to_string_lossy())
            .expect_err("outside the root");
        let missing = access
            .resolve_for_read(&outside.path().join("absent.data").to_string_lossy())
            .expect_err("outside the root");
        assert_eq!(existing.code, missing.code);
        assert_eq!(existing.message, missing.message);
    }

    #[test]
    fn an_extra_allowed_root_is_permitted() -> Result<(), PgError> {
        let data = tempfile::tempdir().expect("temp dir");
        let fixtures = tempfile::tempdir().expect("temp dir");
        let access = CopyFileAccess::confined_to(data.path()).allowing(fixtures.path());
        let resolved =
            access.resolve_for_read(&fixtures.path().join("x.data").to_string_lossy())?;
        assert!(resolved.starts_with(normalize(fixtures.path())));
        Ok(())
    }

    #[test]
    fn deny_all_permits_nothing() {
        let access = CopyFileAccess::deny_all();
        assert!(access.resolve_for_read("/etc/passwd").is_err());
        assert!(access.resolve_for_read("relative.data").is_err());
    }

    #[test]
    fn a_symlink_out_of_a_root_is_denied() {
        let data = tempfile::tempdir().expect("temp dir");
        let outside = tempfile::tempdir().expect("temp dir");
        let target = outside.path().join("secret.data");
        std::fs::write(&target, b"1\n").expect("write fixture");
        let link = data.path().join("link.data");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        #[cfg(not(unix))]
        return;

        let access = CopyFileAccess::confined_to(data.path());
        let err = access
            .resolve_for_read(&link.to_string_lossy())
            .expect_err("a symlink may not smuggle a path out of the root");
        assert_eq!(err.code, sqlstate::INSUFFICIENT_PRIVILEGE);
    }
}
