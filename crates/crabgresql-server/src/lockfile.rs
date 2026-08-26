//! `postmaster.pid`: the file that says a data directory is open, and the one
//! thing that keeps two servers out of one cluster.
//!
//! Nothing used to. Two `crabgresql -D pgdata` on the same directory each ran
//! crash recovery over the same WAL, each published its own redo point into the
//! same `global/pg_control`, and each wrote the same relation files — the one
//! mistake here that destroys data rather than merely making a mess.
//! [`PG_VERSION`](crate::initdb) answers *whose* directory this is and of what
//! version; it cannot answer whether somebody has it open right now.
//!
//! The interlock is PostgreSQL's: a file created with `O_EXCL`, holding the
//! server's PID, checked with `kill(pid, 0)` when it turns out to be there
//! already. Its limits are PostgreSQL's too, and worth stating:
//!
//! - **PID namespaces.** Two containers sharing a bind-mounted data directory
//!   see unrelated PID spaces, so the number in the file means nothing across
//!   them. Neither server would refuse. This is what an advisory `flock` would
//!   buy, at the cost of behaving unlike PostgreSQL on filesystems where locks
//!   are unreliable (NFS) — the interlock people already reason about wins.
//! - **A stale file is normal.** `Drop` unlinks it, and `SIGKILL` or a panic
//!   skips `Drop`, so a *complete* file left by a dead server is an expected
//!   state rather than an error: it is taken over, not refused.
//! - **Only a complete one.** A file that is empty, unreadable or not ours is
//!   refused, never unlinked — PostgreSQL refuses on these too. An empty file
//!   is the state of a server between its `O_EXCL` create and its first write,
//!   so "stale" is not a conclusion that can be drawn from it; the cost is that
//!   a crash inside that window leaves a file an operator has to remove, which
//!   is what the error says to do.
//!
//! Those two together are what makes two live servers impossible, in any
//! interleaving: the loser of a create either sees a live PID, or sees the
//! winner's empty file, and both readings are a refusal. Which is also why
//! `Drop` does not check whose PID the file carries before unlinking — it
//! cannot be anyone else's.
//!
//! The contents are PostgreSQL's eight lines, in PostgreSQL's order, so that an
//! operator reads a file they already know and a `pg_ctl`-shaped tool finds the
//! PID and the status where it looks for them.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crabgresql_wal::sync_dir;

use crate::initdb::{at, create_new_private};

/// The lock file's name, as PostgreSQL spells it: it serves the same purpose,
/// and an operator should not have to learn a second name for it.
pub const LOCK_FILE: &str = "postmaster.pid";

/// Line 8 while the server is still opening its engine and binding its port.
const STATUS_STARTING: &str = "starting";
/// Line 8 once it is accepting connections.
///
/// Padded to [`STATUS_STARTING`]'s width — as PostgreSQL pads its own
/// `PM_STATUS_READY` — so that promoting one to the other is a write at a fixed
/// offset that cannot change the file's length. PostgreSQL's `pg_ctl` trims the
/// line before comparing it, and so does anything else that reads a text file
/// line.
const STATUS_READY: &str = "ready   ";

const _: () = assert!(STATUS_STARTING.len() == STATUS_READY.len());

/// How many times [`PostmasterLock::acquire`] will remove a stale file and try
/// again. One retry: the second `AlreadyExists` means somebody recreated the
/// file between our unlink and our create, and that somebody is a live server —
/// looping would be a race two processes could keep each other in.
const TAKEOVER_ATTEMPTS: usize = 2;

/// What the server knows about itself when it takes the lock, for the lines of
/// the file that describe the running instance rather than the directory.
#[derive(Clone, Debug)]
pub struct LockInfo {
    /// The port the server will listen on. Zero when nothing will listen —
    /// `initdb`, which takes the lock only to keep from working on a live
    /// cluster.
    pub port: u16,
    /// The address it accepts connections on, or `*` for a wildcard bind, as
    /// PostgreSQL writes `listen_addresses`. Empty when nothing will listen.
    pub listen_address: String,
    /// When the process started, in seconds since the epoch.
    pub start_epoch_secs: i64,
}

impl LockInfo {
    /// The lock a command that only touches the directory takes: no listener, so
    /// no port and no address.
    pub fn for_initdb(start_epoch_secs: i64) -> Self {
        LockInfo {
            port: 0,
            listen_address: String::new(),
            start_epoch_secs,
        }
    }
}

/// A held `postmaster.pid`. The file is removed when this is dropped, which is
/// every path out of a server that was not killed outright.
#[derive(Debug)]
pub struct PostmasterLock {
    path: PathBuf,
    file: File,
    /// Where line 8 begins, so [`PostmasterLock::mark_ready`] does not have to
    /// re-derive it from the contents it wrote.
    status_offset: u64,
}

impl PostmasterLock {
    /// Claim `data_dir` for this process, or say who already has it.
    ///
    /// The directory must exist: the lock lives inside it. A caller that may be
    /// pointed at an absent directory — `initdb` — gets `NotFound` and can treat
    /// it as "nothing to lock out", since a server cannot be running in a
    /// directory that is not there.
    pub fn acquire(data_dir: &Path, info: &LockInfo) -> io::Result<Self> {
        let path = data_dir.join(LOCK_FILE);
        for _ in 0..TAKEOVER_ATTEMPTS {
            match create_new_private(&path) {
                Ok(file) => return Self::fill(path, file, data_dir, info),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    take_over(&path, data_dir)?;
                }
                Err(error) => return Err(at(&path, "creating", error)),
            }
        }
        // The file was recreated between our unlink and our create. Whoever did
        // that is running now, whatever the file happens to carry — and if it
        // cannot be read at all, that is still not our directory to take.
        let pid = match read_holder(&path) {
            Ok(Holder::Pid(pid)) => Some(pid),
            _ => None,
        };
        Err(occupied(&path, data_dir, pid))
    }

    /// Write the eight lines and make them durable.
    fn fill(path: PathBuf, file: File, data_dir: &Path, info: &LockInfo) -> io::Result<Self> {
        // Built as one string so the offset of line 8 is known without asking
        // the file where it ended up.
        let head = format!(
            "{pid}\n{dir}\n{start}\n{port}\n{socket}\n{listen}\n{shmem}\n",
            pid = std::process::id(),
            dir = data_dir.display(),
            start = info.start_epoch_secs,
            port = info.port,
            // No unix-domain sockets yet, so there is no directory holding one.
            socket = "",
            listen = info.listen_address,
            // No shared memory: this server is one process, not a postmaster and
            // its children, so there is no segment to name.
            shmem = "0  0",
        );
        let status_offset = head.len() as u64;
        let lock = PostmasterLock {
            path,
            file,
            status_offset,
        };
        // Written through the guard — `&File` is a `Write` — so a failure below
        // drops it, and the half-written file goes with it.
        let mut sink = &lock.file;
        sink.write_all(head.as_bytes())
            .and_then(|()| sink.write_all(STATUS_STARTING.as_bytes()))
            .and_then(|()| sink.write_all(b"\n"))
            .and_then(|()| lock.file.sync_all())
            .map_err(|e| at(&lock.path, "writing", e))?;
        // The file's *name* is the interlock, so the directory entry has to be
        // durable too — the same reason `PG_VERSION` syncs its directory.
        sync_dir(data_dir).map_err(|e| at(data_dir, "syncing", e))?;
        Ok(lock)
    }

    /// Promote line 8 from `starting` to `ready`, once the listener is up.
    ///
    /// A fixed-width overwrite: the rest of the file is untouched and its length
    /// does not change, so a reader can never catch a truncated file.
    pub fn mark_ready(&self) -> io::Result<()> {
        let mut sink = &self.file;
        sink.seek(SeekFrom::Start(self.status_offset))
            .and_then(|_| sink.write_all(STATUS_READY.as_bytes()))
            .and_then(|()| self.file.sync_data())
            .map_err(|e| at(&self.path, "writing", e))
    }

    /// The file this lock is held on, for a caller that wants to name it.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove a lock file this process holds, by path.
    ///
    /// [`Drop`] is the ordinary way, and it is enough for every path that
    /// *returns*. This exists for the one that does not: a shutdown signal
    /// during startup ends the process with `exit`, which runs no destructors,
    /// and a server that has to be stopped before it ever opened its cluster
    /// should not leave a file behind that the next start has to reason about.
    ///
    /// A failure is logged rather than propagated: what is left behind is the
    /// stale file [`PostmasterLock::acquire`] already knows how to take over.
    pub fn release_at(path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "could not remove the lock file; the next server will take it over"
            ),
        }
    }
}

impl Drop for PostmasterLock {
    fn drop(&mut self) {
        Self::release_at(&self.path);
    }
}

/// Decide what an existing lock file means, and clear it if — and only if — it
/// is the remnant of a server that is no longer running.
///
/// `Ok(())` means the file is gone and the caller may create its own. Every
/// other reading is an error, because every other reading is either a live
/// server or a file nobody here can account for: the one outcome worth ruling
/// out absolutely is two servers over one cluster, and an unlink is how that
/// happens.
fn take_over(path: &Path, data_dir: &Path) -> io::Result<()> {
    let pid = match read_holder(path)? {
        // Somebody cleared it between the failed create and this read, which is
        // the state the caller wanted; it retries the create.
        Holder::Vanished => return Ok(()),
        Holder::Empty => return Err(empty(path)),
        Holder::Bogus(line) => return Err(bogus(path, &line)),
        Holder::Pid(pid) if process_is_alive(pid) => {
            return Err(occupied(path, data_dir, Some(pid)));
        }
        Holder::Pid(pid) => pid,
    };
    match fs::remove_file(path) {
        Ok(()) => {}
        // Same again: gone is what we were after.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(at(path, "removing the stale lock file", error)),
    }
    tracing::info!(
        path = %path.display(),
        pid,
        "removed a stale lock file left by a server that is no longer running"
    );
    Ok(())
}

/// What an existing lock file says, once every way of reading it has been
/// distinguished from every other.
///
/// The distinctions are the point. Folded together — as one `Option<i32>` where
/// `None` meant "unreadable, unparseable or absent" — they turned an EACCES on
/// somebody else's lock file into "no live holder" and unlinked it, and turned
/// the empty file of a server that is *this moment* between its `O_EXCL` create
/// and its first write into the same thing.
enum Holder {
    /// Gone by the time it was read.
    Vanished,
    /// Zero length: either a server is in the window described above, or one
    /// crashed inside it.
    Empty,
    /// A first line that is not a PID, so nothing here wrote it.
    Bogus(String),
    Pid(i32),
}

/// Read an existing lock file, or fail saying why it could not be read.
fn read_holder(path: &Path) -> io::Result<Holder> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Holder::Vanished),
        Err(error) => return Err(at(path, "reading", error)),
    };
    // Bounded: a lock file is under a kilobyte, and a stray file at this path
    // must not be read into memory whole. Lossy, for the same reason the parse
    // below is fallible — a first line that is not UTF-8 is `Bogus`, not a
    // failure to classify.
    let mut bytes = Vec::new();
    file.take(1024)
        .read_to_end(&mut bytes)
        .map_err(|error| at(path, "reading", error))?;
    let text = String::from_utf8_lossy(&bytes);

    let first = text.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return Ok(Holder::Empty);
    }
    match first.parse::<i32>() {
        // A non-positive PID cannot name a process: `kill` reads 0 as "this
        // process group" and -1 as "everything we may signal", so parsing one
        // and asking about it would be asking the wrong question entirely.
        Ok(pid) if pid > 0 => Ok(Holder::Pid(pid)),
        _ => Ok(Holder::Bogus(first.to_string())),
    }
}

/// A lock file of zero length. PostgreSQL refuses on this too, and for the
/// reason the hint gives: it is indistinguishable from a server that has just
/// created its own, so treating it as stale is how two servers end up sharing a
/// cluster.
fn empty(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "lock file \"{}\" is empty — either another server is starting, or one \
             crashed while starting; remove it if no server is running",
            path.display()
        ),
    )
}

/// A lock file this server did not write. Left alone: it belongs to whatever
/// put it there, and an operator who has read this can remove it in one command.
fn bogus(path: &Path, line: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "bogus data in lock file \"{}\": \"{line}\" — remove it if no server is running",
            path.display()
        ),
    )
}

/// The error a second server gets. Two sentences, because the first says what is
/// on disk and the second says what to do about it.
fn occupied(path: &Path, data_dir: &Path, pid: Option<i32>) -> io::Error {
    let who = match pid {
        Some(pid) => format!("(PID {pid}) "),
        None => String::new(),
    };
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "lock file \"{}\" already exists — is another crabgresql server {who}running in \
             data directory \"{}\"?",
            path.display(),
            data_dir.display()
        ),
    )
}

/// Whether a process with this id exists.
///
/// `EPERM` counts as alive: the process is there, it just belongs to another
/// account — which is exactly the case where starting a second server over its
/// data directory would be worst.
#[cfg(unix)]
fn process_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 delivers nothing; it only performs the
    // existence and permission check, and touches no memory of ours.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied
}

/// Without a way to ask, every lock file is assumed to be live: refusing to
/// start is recoverable by hand, and opening a cluster somebody else has open is
/// not.
#[cfg(not(unix))]
fn process_is_alive(_pid: i32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> LockInfo {
        LockInfo {
            port: 5433,
            listen_address: "127.0.0.1".to_string(),
            start_epoch_secs: 1_700_000_000,
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .expect("the lock file")
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A PID nothing can be running under: above the largest `pid_max` any of
    /// these platforms allows, so it cannot collide with a live process.
    const DEAD_PID: i32 = 4_194_304 + 1;

    #[test]
    fn the_file_holds_postgresqls_eight_lines() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let lock = PostmasterLock::acquire(dir.path(), &info()).expect("an unlocked directory");

        let lines = lines(&lock.path);
        assert_eq!(lines.len(), 8, "eight lines: {lines:?}");
        assert_eq!(lines[0], std::process::id().to_string());
        assert_eq!(lines[1], dir.path().display().to_string());
        assert_eq!(lines[2], "1700000000");
        assert_eq!(lines[3], "5433");
        assert_eq!(lines[4], "", "no unix socket directory");
        assert_eq!(lines[5], "127.0.0.1");
        assert_eq!(lines[6], "0  0", "no shared memory segment");
        assert_eq!(lines[7].trim(), "starting");
    }

    #[test]
    fn mark_ready_rewrites_the_status_in_place() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let lock = PostmasterLock::acquire(dir.path(), &info()).expect("an unlocked directory");
        let before = fs::metadata(&lock.path).expect("metadata").len();

        lock.mark_ready().expect("the status line is writable");

        let lines = lines(&lock.path);
        assert_eq!(lines.len(), 8, "still eight lines: {lines:?}");
        assert_eq!(lines[7].trim(), "ready");
        assert_eq!(
            fs::metadata(&lock.path).expect("metadata").len(),
            before,
            "the rewrite must not change the file's length"
        );
    }

    #[test]
    fn a_second_acquire_names_the_holder() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let _held = PostmasterLock::acquire(dir.path(), &info()).expect("an unlocked directory");

        let error = PostmasterLock::acquire(dir.path(), &info()).expect_err("a locked directory");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        let text = error.to_string();
        assert!(text.contains("already exists"), "unhelpful error: {text}");
        assert!(
            text.contains(&format!("PID {}", std::process::id())),
            "the error must name the holder: {text}"
        );
        assert!(
            text.contains(&dir.path().display().to_string()),
            "the error must name the directory: {text}"
        );
    }

    /// The failed attempt above must not take the *holder's* file with it when
    /// its own guard is dropped — it never had one, and a lock that unlinks
    /// somebody else's file is worse than no lock at all.
    #[test]
    fn a_refused_acquire_leaves_the_holders_file_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let held = PostmasterLock::acquire(dir.path(), &info()).expect("an unlocked directory");

        let _refused = PostmasterLock::acquire(dir.path(), &info());

        assert!(held.path.exists(), "the holder's lock file was removed");
        assert_eq!(lines(&held.path)[0], std::process::id().to_string());
    }

    #[test]
    fn dropping_the_lock_releases_the_directory() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = {
            let lock = PostmasterLock::acquire(dir.path(), &info()).expect("an unlocked directory");
            lock.path.clone()
        };
        assert!(!path.exists(), "the lock file outlived its guard");

        PostmasterLock::acquire(dir.path(), &info()).expect("a released directory");
    }

    /// What a `SIGKILL`ed server leaves behind. Taken over, not refused.
    #[test]
    fn a_stale_file_is_taken_over() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join(LOCK_FILE);
        fs::write(&path, format!("{DEAD_PID}\n{}\n", dir.path().display())).expect("a stale file");

        let lock =
            PostmasterLock::acquire(dir.path(), &info()).expect("a stale lock is not a lock");

        assert_eq!(lines(&lock.path)[0], std::process::id().to_string());
    }

    /// An empty file is what a server looks like between its `O_EXCL` create and
    /// its first write. Taking it over would put two servers in one cluster, so
    /// it is refused — and the message has to say what to do about the other
    /// case, a crash inside that window.
    #[test]
    fn an_empty_file_is_refused_as_a_server_that_may_be_starting() {
        for contents in ["", "\n", "   \n"] {
            let dir = tempfile::tempdir().expect("a temp dir");
            let path = dir.path().join(LOCK_FILE);
            fs::write(&path, contents).expect("an empty file");

            let error = match PostmasterLock::acquire(dir.path(), &info()) {
                Ok(_) => panic!("{contents:?} must not be taken over"),
                Err(error) => error,
            };

            let text = error.to_string();
            assert!(text.contains("is empty"), "unhelpful error: {text}");
            assert!(
                text.contains("another server is starting"),
                "the error must say what the file might be: {text}"
            );
            assert!(path.exists(), "an empty lock file must not be removed");
        }
    }

    /// A file with a first line nothing here wrote: somebody's editor, another
    /// tool, a half-written something. Not evidence of a live server, but not
    /// ours to unlink either.
    #[test]
    fn a_bogus_file_is_refused_and_left_alone() {
        for contents in ["not a pid\n", "0\n", "-1\n", "\u{feff}42\n"] {
            let dir = tempfile::tempdir().expect("a temp dir");
            let path = dir.path().join(LOCK_FILE);
            fs::write(&path, contents).expect("a bogus file");

            let error = match PostmasterLock::acquire(dir.path(), &info()) {
                Ok(_) => panic!("{contents:?} must not be taken over"),
                Err(error) => error,
            };

            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            let text = error.to_string();
            assert!(text.contains("bogus data"), "unhelpful error: {text}");
            assert!(path.exists(), "a bogus lock file must not be removed");
            assert_eq!(
                fs::read_to_string(&path).expect("still there"),
                contents,
                "the file must be untouched"
            );
        }
    }

    /// The regression this test exists for: a lock file that cannot be *read*
    /// used to classify as "no live holder" and be unlinked. On a data directory
    /// owned by another account — the case `initdb` deliberately tolerates —
    /// that is a second server over a cluster that is already open.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_an_error_rather_than_a_takeover() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` reads one integer out of the kernel and touches
        // nothing of ours. Root can read a `0000` file, which would make this
        // test assert the opposite of what it is for.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join(LOCK_FILE);
        fs::write(&path, "424242\n").expect("a lock file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");

        let error = PostmasterLock::acquire(dir.path(), &info())
            .expect_err("an unreadable file is an error");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains(LOCK_FILE),
            "the error must name the file: {error}"
        );
        assert!(path.exists(), "an unreadable lock file must not be removed");
    }

    /// The lock file is created inside the data directory, so an absent one is
    /// reported as such — `initdb` reads that as "nothing to lock out".
    #[test]
    fn an_absent_directory_is_not_found() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let error = PostmasterLock::acquire(&dir.path().join("nope"), &info())
            .expect_err("no directory, no lock");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_live_process_is_alive_and_a_dead_one_is_not() {
        assert!(
            process_is_alive(std::process::id() as i32),
            "this process is running"
        );
        assert!(!process_is_alive(DEAD_PID));
        assert!(!process_is_alive(0), "0 means the whole process group");
        assert!(
            !process_is_alive(-1),
            "-1 means every process we may signal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("a temp dir");
        let lock = PostmasterLock::acquire(dir.path(), &info()).expect("an unlocked directory");
        let mode = fs::metadata(&lock.path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the lock file must not be readable");
    }
}
