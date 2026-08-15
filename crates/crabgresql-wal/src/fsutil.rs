//! Filesystem durability helpers shared by everything that renames or creates a
//! file it later has to find again.
//!
//! Renaming a file durably takes two steps: fsync the file, then fsync the
//! *directory*, or the new name can be lost while its contents survive. The
//! second step is the one filesystems disagree about, which is what this module
//! exists to normalize.

use std::path::Path;

/// Positional file I/O — an offset-addressed read or write that leaves the
/// file cursor where it found it. Every caller in the crate imports the trait
/// from here rather than naming a platform module, because the platform that
/// provides it differs.
#[cfg(unix)]
pub use std::os::unix::fs::FileExt;
#[cfg(target_family = "wasm")]
pub use wasm_positional::FileExt;

/// `pread`/`pwrite` for wasm targets.
///
/// WASI *has* positional I/O (`fd_pread`/`fd_pwrite`), but std exposes it only
/// behind the unstable `wasi_ext` feature, and this project builds on stable.
/// So the offset is applied with a seek instead.
///
/// A seek-then-read pair is not the same primitive as a `pread`: it moves the
/// cursor, so two threads sharing a `File` would corrupt each other's offsets.
/// That is sound here only because the wasm targets we build for are
/// single-threaded — `wasm32-wasip1-threads` is deliberately not among them,
/// and the buffer-flush worker is compiled out on wasm for the same reason.
#[cfg(target_family = "wasm")]
mod wasm_positional {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};

    pub trait FileExt {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()>;
        fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()>;
    }

    impl FileExt for File {
        fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
            // `&File` implements `Read`/`Write`/`Seek`, so none of this needs a
            // `&mut File` — matching the shape of the traits being stood in for.
            let mut handle = self;
            handle.seek(SeekFrom::Start(offset))?;
            handle.read_exact(buf)
        }

        fn write_all_at(&self, buf: &[u8], offset: u64) -> std::io::Result<()> {
            let mut handle = self;
            handle.seek(SeekFrom::Start(offset))?;
            handle.write_all(buf)
        }
    }
}

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
    //   EPERM  — `ErrorKind::PermissionDenied` would also swallow EACCES,
    //            which is a real misconfiguration and must stay fatal;
    //   EBADF  — has no stable `ErrorKind` at all (it decodes to the unstable
    //            `Uncategorized`), so this is the only way to reach it; some
    //            filesystems return it for fsync on a directory fd opened
    //            read-only;
    //   EINVAL — `ErrorKind::InvalidInput` is far broader than "this fd cannot
    //            be synced".
    // The numbers themselves are per-target: on unix all three sit in the
    // historical low errno block and are identical everywhere, while WASI
    // numbers its errnos alphabetically from scratch — where unix's EPERM(1) is
    // WASI's E2BIG and unix's EBADF(9) is WASI's EBADMSG. Sharing one table
    // would tolerate the wrong errors on the other platform.
    matches!(error.raw_os_error(), Some(n) if TOLERATED_ERRNOS.contains(&n))
}

/// EPERM, EBADF, EINVAL.
#[cfg(unix)]
const TOLERATED_ERRNOS: &[i32] = &[1, 9, 22];

/// EBADF, EINVAL, ENOSYS, EPERM, ENOTCAPABLE.
///
/// Two entries have no unix counterpart in this list: `ENOSYS`, because a WASI
/// host is free to simply not implement `sync`, and `ENOTCAPABLE`, which is the
/// sandbox refusing a right the preopen never granted — the same "the host will
/// not do this" case the whole tolerance exists for. A genuinely missing
/// directory is `ENOENT` and still propagates.
#[cfg(target_os = "wasi")]
const TOLERATED_ERRNOS: &[i32] = &[8, 28, 52, 63, 76];

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
    #[cfg(unix)]
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

    /// The WASI half of the table above. The numbers overlap the unix ones with
    /// entirely different meanings, which is exactly why each target gets its
    /// own list: WASI's EINVAL(28) is unix's ENOSPC, and unix's EINVAL(22) is
    /// WASI's EFBIG.
    #[cfg(target_os = "wasi")]
    #[test]
    fn only_unsupported_directory_fsync_errors_are_tolerated() {
        let tolerated = [
            std::io::Error::from(std::io::ErrorKind::Unsupported),
            std::io::Error::from_raw_os_error(8),  // EBADF
            std::io::Error::from_raw_os_error(28), // EINVAL
            std::io::Error::from_raw_os_error(52), // ENOSYS
            std::io::Error::from_raw_os_error(63), // EPERM
            std::io::Error::from_raw_os_error(76), // ENOTCAPABLE
        ];
        for error in tolerated {
            assert!(is_unsupported(&error), "should be tolerated: {error:?}");
        }

        let fatal = [
            std::io::Error::from_raw_os_error(2),  // EACCES
            std::io::Error::from_raw_os_error(29), // EIO
            std::io::Error::from_raw_os_error(44), // ENOENT
            std::io::Error::from_raw_os_error(51), // ENOSPC
            std::io::Error::from_raw_os_error(69), // EROFS
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
