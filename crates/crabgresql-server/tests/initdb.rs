//! Creating a cluster: what `crabgresql initdb` leaves on disk, and which
//! directories the server agrees to open.
//!
//! The cases that matter here are the refusals. A data directory is identified
//! by one file, and every mistake this module exists to catch — a typo in `-D`,
//! a cluster from another version, a half-created one — is a directory that
//! *almost* looks right.

use std::fs;
use std::path::Path;

use crabgresql_server::initdb::{self, InitOptions, Outcome, PG_VERSION_FILE};

/// Everything a cluster is made of, checked by opening it rather than by
/// listing what we just wrote.
fn assert_is_cluster(dir: &Path) {
    for sub in ["base", "global", "pg_wal", "pg_xact", "stats", "parquet"] {
        assert!(
            dir.join(sub).is_dir(),
            "{sub} should exist under the cluster"
        );
    }
    // Roles are a cluster object, so the catalog holding them is part of what a
    // cluster is. Before `initdb` wrote it, a stamped directory could still be
    // missing one, and whichever server opened it first got to invent its
    // `pg_authid`.
    assert!(
        dir.join("crabgresql_authid").is_file(),
        "the role catalog should exist under the cluster"
    );
    let version = fs::read_to_string(dir.join(PG_VERSION_FILE)).expect("a version stamp");
    assert_eq!(
        version,
        format!("{}\n", crabgresql_types::version::MAJOR_VERSION),
        "the stamp carries this server's major version, newline included"
    );
    crabgresql_wal::read_control(dir)
        .expect("the control file should be readable")
        .expect("initdb publishes a control file");
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn initdb_creates_a_cluster_the_engine_can_open() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    // A directory that does not exist yet: `initdb -D ./pgdata` on a fresh host
    // is the common case, and it must not require a `mkdir` first.
    let dir = parent.path().join("pgdata");

    assert_eq!(
        initdb::init_data_dir(&dir, &InitOptions::default())?,
        Outcome::Created
    );
    assert_is_cluster(&dir);

    // The proof that the skeleton is the *right* skeleton: the engine opens it,
    // runs recovery over an empty log, and checkpoints.
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(&dir)?;
    drop((engine, txnmgr));

    Ok(())
}

/// Everything under a data directory is readable as plain bytes, so the
/// directory's mode is the only thing between a local account and every table.
#[cfg(unix)]
#[test]
fn a_cluster_is_owner_only() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let dir = parent.path().join("pgdata");
    initdb::init_data_dir(&dir, &InitOptions::default())?;

    assert_eq!(mode(&dir), 0o700, "the data directory");
    for sub in ["base", "global", "pg_wal", "pg_xact", "stats", "parquet"] {
        assert_eq!(mode(&dir.join(sub)), 0o700, "{sub}");
    }
    assert_eq!(mode(&dir.join(PG_VERSION_FILE)), 0o600, "the version stamp");

    Ok(())
}

/// An existing directory handed over by a container runtime or an installer
/// arrives world-readable; initializing it has to correct that, or the
/// restriction is advisory.
#[cfg(unix)]
#[test]
fn an_existing_empty_directory_has_its_mode_corrected() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))?;

    assert_eq!(
        initdb::init_data_dir(dir.path(), &InitOptions::default())?,
        Outcome::Created
    );
    assert_eq!(mode(dir.path()), 0o700);

    Ok(())
}

/// `lost+found` is the mount point's, not the cluster's — without this
/// exception no ext4 mount could be a data directory.
#[test]
fn a_directory_holding_only_lost_and_found_is_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    fs::create_dir(dir.path().join("lost+found"))?;

    assert_eq!(
        initdb::init_data_dir(dir.path(), &InitOptions::default())?,
        Outcome::Created
    );
    assert_is_cluster(dir.path());

    Ok(())
}

/// A server killed before it wrote anything else leaves a lock file and nothing
/// more. That directory is still an empty one: refusing it as "occupied by
/// something that is not a cluster" would need an `rm` nobody could guess.
#[test]
fn a_directory_holding_only_a_lock_file_is_empty() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join(crabgresql_server::lockfile::LOCK_FILE),
        "424242\n",
    )?;

    assert_eq!(
        initdb::init_data_dir(dir.path(), &InitOptions::default())?,
        Outcome::Created
    );
    assert_is_cluster(dir.path());

    Ok(())
}

#[test]
fn initdb_refuses_a_directory_that_already_holds_a_cluster() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    initdb::init_data_dir(dir.path(), &InitOptions::default())?;
    let control_before = fs::read(crabgresql_wal::control_path(dir.path()))?;

    let error = initdb::init_data_dir(dir.path(), &InitOptions::default())
        .expect_err("a second initdb must not silently succeed");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(
        error.to_string().contains("already contains"),
        "unhelpful: {error}"
    );
    assert_eq!(
        fs::read(crabgresql_wal::control_path(dir.path()))?,
        control_before,
        "the refused call must not have written anything"
    );

    // The server, unlike initdb, expects to find a cluster there.
    assert_eq!(
        initdb::ensure_initialized(dir.path(), crabgresql_server::DEFAULT_SUPERUSER)?,
        Outcome::AlreadyInitialized
    );

    Ok(())
}

/// The typo case: `-D ~/src` must be an error, not a new cluster scattered
/// through somebody's source tree.
#[test]
fn a_non_empty_foreign_directory_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    fs::write(dir.path().join("notes.txt"), "not a database")?;

    for result in [
        initdb::init_data_dir(dir.path(), &InitOptions::default()),
        initdb::ensure_initialized(dir.path(), crabgresql_server::DEFAULT_SUPERUSER),
    ] {
        let error = result.expect_err("a foreign directory must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("is not a crabgresql data directory"),
            "unhelpful: {error}"
        );
    }
    assert!(
        !dir.path().join("base").exists(),
        "a refused directory must be left alone"
    );

    Ok(())
}

/// Lay out what a PostgreSQL data directory looks like from here: the same
/// `PG_VERSION` this build stamps (PG 19 is the parity target), and a control
/// file we did not write. `stamped` says whether the version file is there at
/// all — a directory whose stamp was removed must not pass for one of our own
/// pre-stamp clusters either.
fn fake_postgres_cluster(dir: &Path, stamped: bool) -> anyhow::Result<Vec<u8>> {
    fs::create_dir_all(dir.join("global"))?;
    fs::create_dir_all(dir.join("base/1"))?;
    // PostgreSQL's `pg_control` opens with a 64-bit system identifier, so its
    // first bytes are effectively random — never our magic.
    let control: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
    fs::write(crabgresql_wal::control_path(dir), &control)?;
    if stamped {
        fs::write(
            dir.join(PG_VERSION_FILE),
            format!("{}\n", crabgresql_types::version::MAJOR_VERSION),
        )?;
    }
    Ok(control)
}

/// The refusal that protects somebody *else's* data. A PostgreSQL 19 cluster
/// carries the same stamp we do, so without the control file's magic we would
/// open one — and the startup checkpoint would overwrite its `pg_control`.
#[test]
fn a_postgresql_data_directory_is_refused_intact() -> anyhow::Result<()> {
    for stamped in [true, false] {
        let dir = tempfile::tempdir()?;
        let control = fake_postgres_cluster(dir.path(), stamped)?;

        for result in [
            initdb::init_data_dir(dir.path(), &InitOptions::default()),
            initdb::ensure_initialized(dir.path(), crabgresql_server::DEFAULT_SUPERUSER),
        ] {
            let error = result.expect_err("somebody else's cluster must be refused");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(
                error
                    .to_string()
                    .contains("looks like a PostgreSQL data directory"),
                "unhelpful (stamped: {stamped}): {error}"
            );
        }

        assert_eq!(
            fs::read(crabgresql_wal::control_path(dir.path()))?,
            control,
            "the refused directory's control file must be untouched"
        );
        assert_eq!(
            dir.path().join(PG_VERSION_FILE).exists(),
            stamped,
            "a refusal must not stamp the directory either"
        );
    }

    Ok(())
}

/// The other side of that check: a control file of *ours* that no longer
/// verifies still belongs to us. `read_control` folds it into "absent" and
/// recovery replays the whole stream — refusing instead would turn a
/// recoverable cluster into an unopenable one.
#[test]
fn our_own_corrupt_control_file_is_still_ours() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    initdb::init_data_dir(dir.path(), &InitOptions::default())?;

    let path = crabgresql_wal::control_path(dir.path());
    let mut bytes = fs::read(&path)?;
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF; // breaks the CRC, keeps the magic
    fs::write(&path, &bytes)?;

    assert_eq!(
        initdb::ensure_initialized(dir.path(), crabgresql_server::DEFAULT_SUPERUSER)?,
        Outcome::AlreadyInitialized
    );
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(dir.path())?;
    drop((engine, txnmgr));

    Ok(())
}

/// `-D /srv/db/pgdata` on a host where `/srv/db` does not exist yet: the
/// directory a person types is created with its parents, as PostgreSQL's
/// `initdb` does.
#[test]
fn a_data_directory_is_created_with_its_parents() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let dir = parent.path().join("srv/db/pgdata");

    assert_eq!(
        initdb::init_data_dir(&dir, &InitOptions::default())?,
        Outcome::Created
    );
    assert_is_cluster(&dir);

    Ok(())
}

/// And when it cannot be created, the complaint has to name the path — the
/// bare `NotFound` a caller gets from `std::fs` names nothing at all.
#[test]
fn a_path_that_cannot_be_created_names_itself() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    fs::write(parent.path().join("in-the-way"), "occupied")?;
    let dir = parent.path().join("in-the-way/pgdata");

    let error = initdb::init_data_dir(&dir, &InitOptions::default())
        .expect_err("a path under a file cannot be created");
    assert!(
        error.to_string().contains(&dir.display().to_string()),
        "the complaint should name the path: {error}"
    );

    Ok(())
}

/// Data directories written before `PG_VERSION` existed hold real data. They
/// are stamped and opened, not refused — the on-disk format is a compatibility
/// boundary.
#[test]
fn a_cluster_from_before_the_version_stamp_is_adopted() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // Exactly what a pre-initdb build leaves behind: whatever the engine
    // happened to create, and no stamp.
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(dir.path())?;
    drop((engine, txnmgr));
    assert!(!dir.path().join(PG_VERSION_FILE).exists());

    assert_eq!(
        initdb::ensure_initialized(dir.path(), crabgresql_server::DEFAULT_SUPERUSER)?,
        Outcome::AdoptedLegacy
    );
    assert_is_cluster(dir.path());
    // Stamped once: the next start is an ordinary one.
    assert_eq!(
        initdb::ensure_initialized(dir.path(), crabgresql_server::DEFAULT_SUPERUSER)?,
        Outcome::AlreadyInitialized
    );

    let (engine, txnmgr) = crabgresql_server::open_pg_engine(dir.path())?;
    drop((engine, txnmgr));

    Ok(())
}

/// Adoption has to tighten the directory too. A cluster created before this
/// module existed was never restricted to its owner, and leaving it that way
/// would make `0700` a thing new clusters happen to get rather than a property
/// of a crabgresql cluster.
#[cfg(unix)]
#[test]
fn an_adopted_cluster_is_restricted_too() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(dir.path())?;
    drop((engine, txnmgr));
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))?;

    assert_eq!(
        initdb::ensure_initialized(dir.path(), crabgresql_server::DEFAULT_SUPERUSER)?,
        Outcome::AdoptedLegacy
    );
    assert_eq!(mode(dir.path()), 0o700);

    Ok(())
}

/// A cluster of another major version may decode or may not; guessing wrong
/// corrupts it, so the stamp is checked before anything is opened.
#[test]
fn a_cluster_from_another_version_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    initdb::init_data_dir(dir.path(), &InitOptions::default())?;
    fs::write(dir.path().join(PG_VERSION_FILE), "12\n")?;

    let error = initdb::ensure_initialized(dir.path(), crabgresql_server::DEFAULT_SUPERUSER)
        .expect_err("an incompatible cluster must not be opened");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let message = error.to_string();
    assert!(
        message.contains("initialized by crabgresql 12")
            && message.contains(crabgresql_types::version::MAJOR_VERSION),
        "the complaint should name both versions: {message}"
    );

    Ok(())
}

/// `--no-sync` trades durability for speed; it must not trade away any of the
/// directory's contents.
#[test]
fn no_sync_writes_the_same_cluster() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    initdb::init_data_dir(
        dir.path(),
        &InitOptions {
            sync: false,
            ..InitOptions::default()
        },
    )?;
    assert_is_cluster(dir.path());

    let (engine, txnmgr) = crabgresql_server::open_pg_engine(dir.path())?;
    drop((engine, txnmgr));

    Ok(())
}

/// `--superuser` used to be readable only by the server, so `initdb` left a
/// cluster owned by `postgres` whatever it was given. The name now reaches the
/// catalog `initdb` writes, which is the only place it can still be honored —
/// the server refuses to rename a superuser it finds already stored.
#[test]
fn the_bootstrap_superuser_is_the_one_initdb_was_given() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    initdb::init_data_dir(
        dir.path(),
        &InitOptions {
            superuser: "bob".into(),
            ..InitOptions::default()
        },
    )?;

    // Opened with a *different* name, so a catalog that ignored `initdb` and
    // bootstrapped itself here would show `carol` and fail.
    let roles = crabgresql_server::RoleCatalog::open(dir.path(), "carol")?;
    let bob = roles.lookup("bob").expect("initdb's superuser");
    assert!(bob.superuser && bob.canlogin);
    assert!(roles.lookup("carol").is_none());
    assert!(roles.lookup("postgres").is_none());

    Ok(())
}

/// Without `--pwfile` the superuser has no password, and a role with no
/// password is trusted rather than authenticated. That is the shape every
/// cluster had before `--pwfile` existed.
#[test]
fn a_cluster_without_a_password_file_has_no_password() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    initdb::init_data_dir(dir.path(), &InitOptions::default())?;

    let roles = crabgresql_server::RoleCatalog::open(dir.path(), "ignored")?;
    let superuser = roles
        .lookup(crabgresql_server::DEFAULT_SUPERUSER)
        .expect("the bootstrap superuser");
    assert_eq!(superuser.password, None);
    assert!(!roles.any_password_set());

    Ok(())
}

/// `--pwfile` stores a SCRAM verifier, never the password: the file on disk
/// must not contain the cleartext anywhere.
#[test]
fn a_password_file_becomes_a_scram_verifier() -> anyhow::Result<()> {
    let parent = tempfile::tempdir()?;
    let pwfile = parent.path().join("pw");
    // A trailing newline is what `printf 'secret\n' > pw` leaves, and it is not
    // part of the password.
    fs::write(&pwfile, "secret\nignored second line\n")?;
    let dir = parent.path().join("pgdata");

    initdb::init_data_dir(
        &dir,
        &InitOptions {
            password: Some(initdb::password_from_file(&pwfile)?),
            ..InitOptions::default()
        },
    )?;

    let roles = crabgresql_server::RoleCatalog::open(&dir, "ignored")?;
    let stored = roles
        .lookup(crabgresql_server::DEFAULT_SUPERUSER)
        .expect("the bootstrap superuser")
        .password
        .expect("a password");
    assert!(stored.starts_with("SCRAM-SHA-256$4096:"), "{stored}");
    assert!(roles.any_password_set());

    let raw = fs::read(dir.join("crabgresql_authid"))?;
    assert!(
        !raw.windows(6).any(|w| w == b"secret"),
        "the cleartext password must not reach the disk"
    );

    Ok(())
}

/// An empty `--pwfile` is a mistake, not a request for no password: somebody
/// who passed the flag asked for one, and creating a trusted superuser instead
/// would be the opposite of what they asked for.
#[test]
fn an_empty_password_file_is_refused() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let pwfile = dir.path().join("pw");
    fs::write(&pwfile, "\n")?;

    let error = initdb::password_from_file(&pwfile).expect_err("an empty password file");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        error.to_string().contains(&pwfile.display().to_string()),
        "the complaint should name the file: {error}"
    );

    // And a file that is not there at all names itself too.
    let missing = initdb::password_from_file(&dir.path().join("nope"))
        .expect_err("a password file that does not exist");
    assert!(missing.to_string().contains("nope"), "{missing}");

    Ok(())
}

/// A cluster from before `initdb` wrote the role catalog has roles of its own,
/// stored by the server that created them. Adoption must not replace them.
#[test]
fn adoption_keeps_the_roles_a_legacy_cluster_already_had() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, txnmgr) = crabgresql_server::open_pg_engine(dir.path())?;
    drop((engine, txnmgr));
    // The role catalog such a cluster grew on its first start.
    drop(crabgresql_server::RoleCatalog::open(dir.path(), "bob")?);

    assert_eq!(
        initdb::ensure_initialized(dir.path(), "carol")?,
        Outcome::AdoptedLegacy
    );

    let roles = crabgresql_server::RoleCatalog::open(dir.path(), "dave")?;
    assert!(roles.lookup("bob").is_some(), "the stored role survives");
    assert!(roles.lookup("carol").is_none());

    Ok(())
}

/// The role catalog holds password verifiers, which are offline-attackable by
/// anyone who can read them. The `0700` on the directory is the outer fence;
/// the file's own mode is the inner one.
#[cfg(unix)]
#[test]
fn the_role_catalog_is_owner_only() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir()?;
    initdb::init_data_dir(
        dir.path(),
        &InitOptions {
            password: Some("secret".into()),
            ..InitOptions::default()
        },
    )?;
    assert_eq!(mode(&dir.path().join("crabgresql_authid")), 0o600);

    // And it stays that way after the catalog is rewritten, which is a whole
    // new file every time a role changes.
    let roles = crabgresql_server::RoleCatalog::open(dir.path(), "ignored")?;
    roles.rename_role(crabgresql_server::DEFAULT_SUPERUSER, "bob")?;
    assert_eq!(mode(&dir.path().join("crabgresql_authid")), 0o600);

    // A rewrite goes through a temporary file that is renamed over the
    // catalog, and a rename carries the *temporary* file's mode. One left
    // behind by a crash — or by a build from before this file was owner-only —
    // would otherwise publish the verifiers world-readable, because the mode on
    // `open(2)` applies only when it creates the file.
    let tmp = dir.path().join("crabgresql_authid.tmp");
    fs::write(&tmp, b"leftover")?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644))?;
    roles.rename_role("bob", "carol")?;
    assert_eq!(mode(&dir.path().join("crabgresql_authid")), 0o600);

    Ok(())
}
