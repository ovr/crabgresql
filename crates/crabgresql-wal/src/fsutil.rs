//! Filesystem durability helpers shared by everything that renames or creates a
//! file it later has to find again.
//!
//! Renaming a file durably takes two steps: fsync the file, then fsync the
//! *directory*, or the new name can be lost while its contents survive. The
//! second step is the one filesystems disagree about, which is what this module
//! exists to normalize.

use std::path::Path;

/// fsync `dir` so directory entries created or renamed inside it are durable.
///
/// Tolerates the handful of errors that mean "this filesystem does not support
/// fsync on a directory" rather than "the data is not durable" — some network
/// mounts and container overlay layers reject it outright, and a database that
/// refuses to start on them is worse than one that starts and says so. Every
/// other error propagates: `EIO`, `ENOSPC`, `EROFS` and `ENOENT` are real
/// failures and must not be mistaken for a platform quirk.
pub fn sync_dir(dir: &Path) -> std::io::Result<()> {
    let handle = match std::fs::File::open(dir) {
        Ok(handle) => handle,
        // The open itself is what fails under a sandbox, so it needs the same
        // tolerance as the fsync below.
        Err(error) if is_unsupported(&error) => return report(dir, &error),
        Err(error) => return Err(error),
    };
    match handle.sync_all() {
        Ok(()) => Ok(()),
        Err(error) if is_unsupported(&error) => report(dir, &error),
        Err(error) => Err(error),
    }
}

/// Whether `error` means the platform cannot fsync a directory handle.
fn is_unsupported(error: &std::io::Error) -> bool {
    // `ENOTSUP`/`EOPNOTSUPP`/`ENOSYS` are matched through `ErrorKind` precisely
    // because their numbers differ per target (`EOPNOTSUPP` is 95 on Linux, 45 on
    // macOS); std already normalizes them.
    if error.kind() == std::io::ErrorKind::Unsupported {
        return true;
    }
    // The rest are matched by raw number, not `ErrorKind`, on purpose:
    //   EPERM  (1) — `ErrorKind::PermissionDenied` would also swallow EACCES,
    //                which is a real misconfiguration and must stay fatal;
    //   EBADF  (9) — has no stable `ErrorKind` at all (it decodes to the
    //                unstable `Uncategorized`), so this is the only way to
    //                reach it; some filesystems return it for fsync on a
    //                directory fd opened read-only;
    //   EINVAL (22) — `ErrorKind::InvalidInput` is far broader than "this fd
    //                cannot be synced".
    // All three sit in the historical low errno block and are identical on every
    // unix target this crate builds for.
    matches!(error.raw_os_error(), Some(1 | 9 | 22))
}

/// Say so once, loudly, then stay quiet: the control file is rewritten on every
/// checkpoint, so an unconditional warning would be per-checkpoint spam on
/// exactly the filesystems this tolerance exists to support.
fn report(dir: &Path, error: &std::io::Error) -> std::io::Result<()> {
    static ANNOUNCED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !ANNOUNCED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            dir = %dir.display(),
            %error,
            "this filesystem cannot fsync a directory; a crash may lose the most \
             recently created or renamed file name"
        );
    } else {
        tracing::debug!(dir = %dir.display(), %error, "directory fsync unsupported");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classifier is the whole fix, so pin it against a table rather than
    /// against whatever filesystem the tests happen to run on. Widening it to
    /// everything, or narrowing it to `ErrorKind` alone, both go red here.
    #[test]
    fn only_unsupported_directory_fsync_errors_are_tolerated() {
        let tolerated = [
            std::io::Error::from(std::io::ErrorKind::Unsupported),
            std::io::Error::from_raw_os_error(1),  // EPERM
            std::io::Error::from_raw_os_error(9),  // EBADF
            std::io::Error::from_raw_os_error(22), // EINVAL
        ];
        for error in tolerated {
            assert!(is_unsupported(&error), "should be tolerated: {error:?}");
        }

        let fatal = [
            std::io::Error::from_raw_os_error(2),  // ENOENT
            std::io::Error::from_raw_os_error(5),  // EIO
            std::io::Error::from_raw_os_error(13), // EACCES
            std::io::Error::from_raw_os_error(28), // ENOSPC
            std::io::Error::from_raw_os_error(30), // EROFS
        ];
        for error in fatal {
            assert!(!is_unsupported(&error), "should be fatal: {error:?}");
        }
    }

    #[test]
    fn syncing_a_real_directory_succeeds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        sync_dir(dir.path())?;

        Ok(())
    }

    /// A directory that does not exist is `ENOENT` — a real failure, not a
    /// platform quirk, so it must propagate.
    #[test]
    fn syncing_a_missing_directory_fails() {
        let error = sync_dir(Path::new("/nonexistent-crabgresql-fsutil-test"))
            .expect_err("a missing directory must not be tolerated");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}
