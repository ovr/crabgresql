//! Creating a cluster: the data directory's skeleton, its version stamp, and
//! its first `pg_control`.
//!
//! Before this existed, a data directory came into being by accident — whichever
//! component ran first created the subdirectory it needed, and any directory at
//! all was therefore a valid `-D` target. A typo in the path produced a new
//! empty cluster inside somebody else's directory rather than an error, and
//! nothing on disk said which version of the server had written what was there.
//!
//! So a cluster now has a moment of creation, and one marker that says so:
//! `PG_VERSION`, written **last** and fsynced. Everything above it — the
//! subdirectories and `global/pg_control` — is created first, which makes the
//! marker mean "this directory was initialized all the way to the end". A crash
//! partway through leaves a directory that still looks like a cluster to
//! [`inspect`] (a control file, or `base/`), and that is deliberate: the same
//! shape is what every data directory written before this module looks like, and
//! those hold real data. They are adopted — stamped with a `PG_VERSION` — rather
//! than refused, because the on-disk format is a compatibility boundary
//! (`AGENTS.md`) and refusing would mean losing a cluster on upgrade.
//!
//! Two entry points, differing in one answer only: [`init_data_dir`] is the
//! `initdb` subcommand, and refuses a directory that already holds a cluster;
//! [`ensure_initialized`] is what the server calls before opening one, and
//! accepts it. Everything else — what counts as empty, as foreign, as a cluster
//! of the wrong version — is decided once, in [`inspect`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crabgresql_pg_engine::{BASE_SUBDIR, PARQUET_SUBDIR, STATS_SUBDIR};
use crabgresql_txn::Xid;
use crabgresql_txn::clog::CLOG_SUBDIR;
use crabgresql_types::version::MAJOR_VERSION;
use crabgresql_wal::{
    CONTROL_SUBDIR, ControlFile, Lsn, control_is_foreign, control_path, sync_dir, wal_dir,
};

/// The file naming the major version that wrote this data directory. Spelled as
/// PostgreSQL spells it, because it serves the same purpose and an operator
/// looking at a directory should not have to learn a second name for it.
pub const PG_VERSION_FILE: &str = "PG_VERSION";

/// A directory that is empty apart from this is still empty: a filesystem's
/// mount point owns it, not the cluster. PostgreSQL's `initdb` makes the same
/// exception, and without it no ext4 mount could be used as a data directory.
const LOST_AND_FOUND: &str = "lost+found";

/// So is one holding nothing but a lock file — and this rule is load-bearing,
/// not merely tolerant: the lock is taken *before* a cluster is created
/// ([`crate::lockfile`]), so the caller's own `postmaster.pid` is sitting in the
/// directory at the moment [`inspect`] decides whether it is empty. A server
/// killed before it wrote anything else leaves the same shape behind.
const IGNORED_WHEN_EMPTY: [&str; 2] = [LOST_AND_FOUND, crate::lockfile::LOCK_FILE];

/// How much durability the caller wants to pay for while initializing.
#[derive(Clone, Copy, Debug)]
pub struct InitOptions {
    /// `false` is `initdb --no-sync`: fine for a throwaway cluster, and unsafe
    /// for one that will be kept, because a crash right after can leave the
    /// skeleton half-durable.
    ///
    /// It does not reach `global/pg_control`, which is always published
    /// durably — that write is an atomic-publish primitive shared with the
    /// checkpointer, and it is one file.
    pub sync: bool,
}

impl Default for InitOptions {
    fn default() -> Self {
        InitOptions { sync: true }
    }
}

/// What initializing a directory turned out to involve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The directory was absent or empty, and now holds a new cluster.
    Created,
    /// It already held a cluster of this version; nothing was written.
    AlreadyInitialized,
    /// It held a cluster written before `PG_VERSION` existed, which has now been
    /// stamped with one. Its data is untouched.
    AdoptedLegacy,
}

/// What a directory holds, as far as being a data directory goes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Absent,
    /// Empty but for [`LOST_AND_FOUND`], which belongs to the mount point.
    Empty,
    /// Stamped, with the version it carries.
    Initialized(String),
    /// A cluster somebody else wrote: its `pg_control` carries a magic that is
    /// not ours. Almost certainly PostgreSQL's, which stamps the same
    /// `PG_VERSION` this build does — the stamp alone cannot tell them apart.
    ForeignCluster,
    /// A cluster of ours from before `PG_VERSION` was written.
    Legacy,
    /// Occupied by something that is not a cluster at all.
    Foreign,
}

/// Create a cluster in `dir`, as the `initdb` subcommand does.
///
/// Refuses a directory that already holds a cluster of this version: `initdb` is
/// how a cluster is created, so being asked to create one where a cluster
/// already sits is a mistake worth naming rather than a no-op. A directory from
/// before `PG_VERSION` is stamped in place — see the module docs.
pub fn init_data_dir(dir: &Path, opts: &InitOptions) -> io::Result<Outcome> {
    bootstrap(dir, opts, true)
}

/// Make sure `dir` is a cluster this server can open, creating one if the
/// directory is absent or empty. What the server calls before opening the
/// engine.
///
/// Unlike [`init_data_dir`], an existing cluster is the expected case and is
/// left alone.
pub fn ensure_initialized(dir: &Path) -> io::Result<Outcome> {
    bootstrap(dir, &InitOptions::default(), false)
}

/// The shared body of both entry points: `refuse_existing` is the only thing
/// they disagree about.
fn bootstrap(dir: &Path, opts: &InitOptions, refuse_existing: bool) -> io::Result<Outcome> {
    match inspect(dir)? {
        State::Absent | State::Empty => {
            create(dir, opts)?;
            Ok(Outcome::Created)
        }
        State::Initialized(version) if version == MAJOR_VERSION => {
            if refuse_existing {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "directory \"{}\" already contains a crabgresql cluster \
                         ({PG_VERSION_FILE} says {version}); start the server on it, \
                         or choose an empty directory",
                        dir.display()
                    ),
                ));
            }
            Ok(Outcome::AlreadyInitialized)
        }
        // A cluster from another major version. Its files may decode or may not,
        // and a server that guessed wrong would corrupt them: the whole point of
        // the stamp is that this is answered before anything is opened.
        State::Initialized(version) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the data directory \"{}\" was initialized by crabgresql {version}, \
                 which is not compatible with this server ({MAJOR_VERSION})",
                dir.display()
            ),
        )),
        // Refused whether or not it carries a stamp: overwriting somebody else's
        // control file is the one mistake here that destroys data rather than
        // merely making a mess.
        State::ForeignCluster => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "directory \"{}\" holds a {CONTROL_SUBDIR}/pg_control that crabgresql \
                 did not write — it looks like a PostgreSQL data directory, which \
                 this server cannot open",
                dir.display()
            ),
        )),
        State::Legacy => {
            adopt(dir, opts)?;
            Ok(Outcome::AdoptedLegacy)
        }
        State::Foreign => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "directory \"{}\" exists but is not a crabgresql data directory: it \
                 holds files and no {PG_VERSION_FILE}; remove or empty it, or point \
                 --data-dir somewhere else",
                dir.display()
            ),
        )),
    }
}

/// Classify `dir`. The one place any of these questions is answered.
fn inspect(dir: &Path) -> io::Result<State> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::Absent),
        // Anything else is about *this* path — `-D` under a plain file gives a
        // bare "Not a directory" otherwise, naming nothing.
        Err(error) => return Err(at(dir, "reading", error)),
    };

    // Asked once, before anything is accepted. A directory whose control file is
    // not ours is nobody's to initialize, stamped or not: with a stamp it is a
    // PostgreSQL cluster (its `PG_VERSION` matches ours, since PG 19 is the
    // parity target), and without one it would otherwise pass for a cluster of
    // ours from before the stamp existed and be adopted.
    if control_is_foreign(dir).map_err(io::Error::other)? {
        return Ok(State::ForeignCluster);
    }

    let mut occupied = false;
    for entry in entries {
        let name = entry?.file_name();
        if name == PG_VERSION_FILE {
            let stamp = dir.join(PG_VERSION_FILE);
            let raw = fs::read(&stamp).map_err(|error| at(&stamp, "reading", error))?;
            let version = String::from_utf8(raw)
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "\"{}\" is not readable as a version",
                            dir.join(PG_VERSION_FILE).display()
                        ),
                    )
                })?
                .trim()
                .to_string();
            return Ok(State::Initialized(version));
        }
        if !IGNORED_WHEN_EMPTY.iter().any(|ignored| name == *ignored) {
            occupied = true;
        }
    }

    if !occupied {
        return Ok(State::Empty);
    }
    // No stamp, but the shape of a cluster: either a control file, or the heap's
    // relation files. Written by a build from before this module existed.
    if control_path(dir).exists() || dir.join(BASE_SUBDIR).is_dir() {
        return Ok(State::Legacy);
    }
    Ok(State::Foreign)
}

/// Every directory a cluster is made of, in the order they are created. The
/// names come from the components that open them, so a rename there cannot
/// leave this list behind.
fn subdirs(dir: &Path) -> [PathBuf; 5] {
    [
        dir.join(BASE_SUBDIR),
        dir.join(CONTROL_SUBDIR),
        wal_dir(dir),
        dir.join(CLOG_SUBDIR),
        dir.join(STATS_SUBDIR),
    ]
}

/// Write a fresh cluster into `dir`, which is absent or empty.
fn create(dir: &Path, opts: &InitOptions) -> io::Result<()> {
    make_dir_all(dir)?;
    for sub in subdirs(dir) {
        make_dir(&sub)?;
    }
    // Parquet relations get a directory each under this one; the engine creates
    // those, but the root belongs to the skeleton like every other.
    make_dir(&dir.join(PARQUET_SUBDIR))?;

    // `redo_lsn` is invalid because there is no log to resume from, and
    // `next_xid` is the floor `recover` assumes when no control file exists at
    // all. The startup checkpoint overwrites this within moments; what writing
    // it buys is that the very first open reads a file somebody wrote, rather
    // than inferring a fresh cluster from an absent one.
    crabgresql_wal::write_control(
        dir,
        &ControlFile {
            next_xid: Xid::FIRST_NORMAL,
            redo_lsn: Lsn::INVALID,
            // Nothing has run, so nothing crashed: an unlogged relation created
            // by the first session must survive that session's server.
            clean_shutdown: true,
        },
    )
    .map_err(io::Error::other)?;

    stamp_version(dir, opts)
}

/// Stamp a data directory written before `PG_VERSION` existed, leaving its data
/// alone. Missing skeleton directories are filled in on the way, since a build
/// that created them lazily may never have needed one.
fn adopt(dir: &Path, opts: &InitOptions) -> io::Result<()> {
    // The directory itself too: nothing restricted it to its owner back then,
    // and `0700` has to be a property of a crabgresql cluster rather than
    // something only new ones happen to get.
    make_dir_all(dir)?;
    for sub in subdirs(dir) {
        make_dir(&sub)?;
    }
    make_dir(&dir.join(PARQUET_SUBDIR))?;
    stamp_version(dir, opts)
}

/// Write `PG_VERSION`, the marker that makes a directory a cluster. Last, and
/// durable: everything it vouches for must be on disk before it is.
fn stamp_version(dir: &Path, opts: &InitOptions) -> io::Result<()> {
    let path = dir.join(PG_VERSION_FILE);
    {
        use std::io::Write;
        let mut file = create_private(&path).map_err(|error| at(&path, "creating", error))?;
        writeln!(file, "{MAJOR_VERSION}").map_err(|error| at(&path, "writing", error))?;
        if opts.sync {
            file.sync_all()
                .map_err(|error| at(&path, "syncing", error))?;
        }
    }
    if opts.sync {
        sync_dir(dir).map_err(|error| at(dir, "syncing", error))?;
    }
    Ok(())
}

/// Create `path` if it is not there, owner-only.
///
/// `0700` is the mode PostgreSQL gives a data directory, and for the same
/// reason: everything under it is readable as plain bytes, so read access to
/// the directory is read access to every table. An existing directory has its
/// mode corrected too — a data directory handed over by a container runtime or
/// an installer arrives world-readable, and quietly keeping it that way would
/// make the restriction advisory.
///
/// On a directory we did not create, a *refused* chmod is a complaint rather
/// than a failure: a bind mount owned by another account is somebody else's
/// deliberate arrangement, and a server that used to start on it must not stop
/// starting because it could not tighten a mode. Anything we created ourselves
/// we own, so there the same error is real and propagates.
fn make_dir(path: &Path) -> io::Result<()> {
    let created = match fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(at(path, "creating", error)),
    };
    tighten(path, created)
}

/// [`make_dir`] for a path whose parents may not exist either — the data
/// directory itself, which is the one path a person types.
///
/// `-D /srv/db/pgdata` on a host where `/srv/db` does not exist yet has to work:
/// `StorageManager::open` created its directory with `create_dir_all` before
/// this module existed, and PostgreSQL's `initdb` runs `pg_mkdir_p`. The parents
/// keep whatever the umask gives them, as `pg_mkdir_p` leaves them; only the
/// data directory is restricted, because only it holds the tables.
fn make_dir_all(path: &Path) -> io::Result<()> {
    let created = !path.exists();
    if let Err(error) = fs::create_dir_all(path) {
        return Err(at(path, "creating", error));
    }
    tighten(path, created)
}

/// Create the data directory itself, if it is not there — the one step that has
/// to happen before anything else, because the lock file
/// ([`crate::lockfile`]) lives inside it and has to be taken before a single
/// byte of a cluster is written.
///
/// A directory that already exists is left entirely alone, mode included: `-D`
/// with a typo must not `chmod 0700` somebody's home directory on its way to
/// refusing it. One we create ourselves is restricted immediately, by
/// [`make_dir_all`], rather than a moment later when the cluster is written
/// into it.
pub fn create_data_dir_if_absent(dir: &Path) -> io::Result<()> {
    match dir.exists() {
        true => Ok(()),
        false => make_dir_all(dir),
    }
}

/// Restrict `path` to its owner, tolerating a refusal on a directory `created`
/// says we did not make. See [`make_dir`] for why that tolerance exists.
fn tighten(path: &Path, created: bool) -> io::Result<()> {
    match restrict(path) {
        Ok(()) => Ok(()),
        Err(error) if !created && error.kind() == io::ErrorKind::PermissionDenied => {
            tracing::warn!(
                dir = %path.display(),
                %error,
                "could not restrict this directory to its owner; anything that can \
                 read it can read every table in it"
            );
            Ok(())
        }
        Err(error) => Err(at(path, "restricting", error)),
    }
}

/// Put the path back into an error that lost it.
///
/// `ErrorKind` survives, so callers keep matching on it; what changes is that
/// `-D` at a path that cannot be created says *which* path, instead of a bare
/// "No such file or directory" naming nothing.
pub(crate) fn at(path: &Path, what: &str, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{what} \"{}\": {error}", path.display()),
    )
}

/// Create (or truncate) `path` owner-only, in one step — a `create` followed by
/// a `chmod` would leave a window where the file is readable, and everything in
/// a data directory is readable as plain bytes.
fn create_private(path: &Path) -> io::Result<fs::File> {
    private().truncate(true).open(path)
}

/// The same, but failing with `AlreadyExists` rather than truncating what is
/// there. This is the create half of the lock file's interlock
/// ([`crate::lockfile`]), so the exclusion has to be the filesystem's — a
/// `exists()` check followed by a create is two racing servers' happy path.
pub(crate) fn create_new_private(path: &Path) -> io::Result<fs::File> {
    private().create_new(true).open(path)
}

/// The half both share: write access, and `0600` on anything created.
#[cfg(unix)]
fn private() -> fs::OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).mode(0o600);
    opts
}

#[cfg(not(unix))]
fn private() -> fs::OpenOptions {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true);
    opts
}

#[cfg(unix)]
fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> io::Result<()> {
    Ok(())
}
