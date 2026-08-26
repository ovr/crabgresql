//! The cluster's role catalog: `CREATE`/`ALTER`/`DROP ROLE`, role membership,
//! and the flat file the two persist to.
//!
//! Roles are a *cluster* object in PostgreSQL, not a database one: they live in
//! `pg_authid`, which is shared by every database in the cluster and is
//! therefore stored outside any one database's catalog. crabgresql mirrors that
//! shape rather than the relation catalog's: the store here is one file under
//! the data directory, independent of the per-relation catalog that
//! [`crabgresql_pg_engine`] writes, and it is shared by every connection for the
//! life of the server.
//!
//! What this module does *not* do is decide anything: it stores roles, answers
//! membership questions, and reports PostgreSQL's errors. The stored password
//! is a SCRAM verifier, and [`crate::auth`] is what checks a client against
//! one; privilege checks on objects are separate again and do not exist yet.
//! Neither does `rolcanlogin`: a role with `NOLOGIN` can still connect, because
//! nothing consults it at startup.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crabgresql_catalog::BOOTSTRAP_ROLE_OID;
use crabgresql_pg_wire::sqlstate;

use crate::error::PgError;
use crate::global_catalog::CatalogNotice;

/// User-created roles get OIDs at or above this, matching the range every other
/// user object is assigned from (see `global_catalog::FIRST_USER_OID`).
const FIRST_USER_OID: u32 = 16384;

/// OID of the role a session gets when it authenticates as a name the catalog
/// does not have. Below [`FIRST_USER_OID`], so it can never collide with a real
/// role, and distinct from [`BOOTSTRAP_ROLE_OID`], so `pg_get_userbyid` on an
/// object's owner still resolves to the bootstrap superuser rather than to
/// whoever happens to be connected. See [`RoleCatalog::login`].
pub const TRUST_SESSION_ROLE_OID: u32 = 16383;

/// One role, as `pg_authid` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    pub oid: u32,
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub createrole: bool,
    pub createdb: bool,
    pub canlogin: bool,
    pub replication: bool,
    pub bypassrls: bool,
    /// `-1` is PostgreSQL's "no per-role limit".
    pub connlimit: i32,
    /// The SCRAM verifier, or an `md5…` hash a client supplied pre-hashed.
    /// `pg_authid` shows it; `pg_roles`/`pg_user` mask it.
    pub password: Option<String>,
    /// `rolvaliduntil` as `timestamptz` micros.
    pub valid_until: Option<i64>,
    /// `rolconfig`: `ALTER ROLE … SET x = y` entries, spelled `x=y` as
    /// PostgreSQL stores them.
    pub config: Vec<String>,
}

impl Role {
    /// The bootstrap superuser: what `initdb` creates, and the owner OID every
    /// catalog row reports.
    fn bootstrap(name: &str) -> Self {
        Self {
            oid: BOOTSTRAP_ROLE_OID,
            name: name.to_string(),
            superuser: true,
            inherit: true,
            createrole: true,
            createdb: true,
            canlogin: true,
            replication: true,
            bypassrls: true,
            connlimit: -1,
            password: None,
            valid_until: None,
            config: Vec::new(),
        }
    }

    /// A `CREATE ROLE` with no options: PostgreSQL's defaults are "off" for
    /// every attribute except `INHERIT`.
    fn new(oid: u32, name: String) -> Self {
        Self {
            oid,
            name,
            superuser: false,
            inherit: true,
            createrole: false,
            createdb: false,
            canlogin: false,
            replication: false,
            bypassrls: false,
            connlimit: -1,
            password: None,
            valid_until: None,
            config: Vec::new(),
        }
    }

    fn apply(&mut self, opts: &RoleOptions) {
        let RoleOptions {
            superuser,
            inherit,
            createrole,
            createdb,
            canlogin,
            replication,
            bypassrls,
            connlimit,
            password,
            valid_until,
        } = opts;
        if let Some(v) = superuser {
            self.superuser = *v;
        }
        if let Some(v) = inherit {
            self.inherit = *v;
        }
        if let Some(v) = createrole {
            self.createrole = *v;
        }
        if let Some(v) = createdb {
            self.createdb = *v;
        }
        if let Some(v) = canlogin {
            self.canlogin = *v;
        }
        if let Some(v) = replication {
            self.replication = *v;
        }
        if let Some(v) = bypassrls {
            self.bypassrls = *v;
        }
        if let Some(v) = connlimit {
            self.connlimit = *v;
        }
        if let Some(v) = password {
            self.password = v.clone();
        }
        if let Some(v) = valid_until {
            self.valid_until = Some(*v);
        }
    }
}

/// What [`RoleCatalog::login`] found for the name a client connected under.
#[derive(Clone, Debug)]
pub enum Login {
    /// A stored role. Whether it has to prove anything is its `password`: a
    /// role with one must pass SCRAM, a role without one is trusted.
    Known(Role),
    /// No such role, in a cluster where nothing has a password — so there is
    /// nothing this connection could have been asked to prove. The role is
    /// synthetic and lives only for this session.
    Trusted(Role),
    /// No such role, in a cluster where at least one role has a password.
    NoSuchRole,
}

/// One `pg_auth_members` row: `member` is a member of `role`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Membership {
    pub oid: u32,
    pub role: u32,
    pub member: u32,
    pub grantor: u32,
    pub admin_option: bool,
    /// Whether the member inherits `role`'s privileges without `SET ROLE`.
    /// PostgreSQL defaults it to the member's own `rolinherit`.
    pub inherit_option: bool,
    /// Whether the member may `SET ROLE` to `role`.
    pub set_option: bool,
}

/// The attribute changes one `CREATE`/`ALTER ROLE` asks for. `None` means the
/// statement did not mention the attribute; `password`'s inner `None` is the
/// explicit `PASSWORD NULL` that clears the stored verifier. `valid_until` needs
/// no such spelling — PostgreSQL has none that puts `rolvaliduntil` back to
/// NULL, `'infinity'` being an ordinary value there.
#[derive(Clone, Debug, Default)]
pub struct RoleOptions {
    pub superuser: Option<bool>,
    pub inherit: Option<bool>,
    pub createrole: Option<bool>,
    pub createdb: Option<bool>,
    pub canlogin: Option<bool>,
    pub replication: Option<bool>,
    pub bypassrls: Option<bool>,
    pub connlimit: Option<i32>,
    pub password: Option<Option<String>>,
    pub valid_until: Option<i64>,
}

/// A consistent read of the whole catalog, for the system relations. Taken under
/// one lock so `pg_auth_members` can never name a role `pg_authid` does not
/// show.
#[derive(Clone, Debug, Default)]
pub struct RoleSnapshot {
    pub roles: Vec<Role>,
    pub members: Vec<Membership>,
}

#[derive(Clone, Default)]
struct RoleStore {
    roles: Vec<Role>,
    members: Vec<Membership>,
    next_oid: u32,
}

impl RoleStore {
    fn position(&self, name: &str) -> Option<usize> {
        self.roles.iter().position(|r| r.name == name)
    }

    fn oid_of(&self, name: &str) -> Option<u32> {
        self.roles.iter().find(|r| r.name == name).map(|r| r.oid)
    }

    /// The OID of `name`, or PostgreSQL's `42704` naming it.
    fn require(&self, name: &str) -> Result<u32, PgError> {
        self.oid_of(name).ok_or_else(|| undefined_role(name))
    }

    fn name_of(&self, oid: u32) -> String {
        self.roles
            .iter()
            .find(|r| r.oid == oid)
            .map_or_else(|| oid.to_string(), |r| r.name.clone())
    }

    fn alloc_oid(&mut self) -> u32 {
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }

    /// Whether `member` is a member of `role`, directly or through another
    /// role. The walk is over the membership edges, so a grant that would close
    /// a cycle is caught before it is stored and this cannot loop.
    fn is_member_of(&self, role: u32, member: u32) -> bool {
        let mut seen = HashSet::from([member]);
        let mut queue = vec![member];
        while let Some(current) = queue.pop() {
            for edge in self.members.iter().filter(|m| m.member == current) {
                if edge.role == role {
                    return true;
                }
                if seen.insert(edge.role) {
                    queue.push(edge.role);
                }
            }
        }
        false
    }
}

/// The server-lifetime role catalog: every connection shares one.
pub struct RoleCatalog {
    inner: RwLock<RoleStore>,
    /// `None` for the in-memory server, which has no data directory to anchor
    /// the file on — its roles live as long as the process.
    path: Option<PathBuf>,
}

impl RoleCatalog {
    /// Open the catalog stored under `data_dir`.
    ///
    /// [`write_bootstrap`] is what creates the file, from `initdb`. The
    /// fallback here — a single bootstrap superuser named `superuser` — is for
    /// a data directory that predates that, and for callers that open an engine
    /// directly without going through `initdb` at all.
    pub fn open(data_dir: &Path, superuser: &str) -> io::Result<Self> {
        let path = data_dir.join(ROLE_FILE);
        let store = match std::fs::File::open(&path) {
            Ok(mut file) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                decode(&bytes)?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => bootstrap_store(superuser),
            Err(e) => return Err(e),
        };
        let catalog = Self {
            inner: RwLock::new(store),
            path: Some(path),
        };
        // Write the bootstrap file out immediately, so the role set a server
        // starts with is the one a later start reads back — rather than one that
        // depends on whether `--superuser` was passed the same way twice.
        catalog
            .with_store(|_| Ok(()))
            .map_err(|e| io::Error::other(e.message))?;
        Ok(catalog)
    }

    /// A catalog that is never written anywhere, holding just the bootstrap
    /// superuser. For the in-memory server and for tests.
    pub fn in_memory(superuser: &str) -> Self {
        Self {
            inner: RwLock::new(bootstrap_store(superuser)),
            path: None,
        }
    }

    /// Everything the system relations need, read under one lock.
    pub fn snapshot(&self) -> RoleSnapshot {
        let store = self.read();
        RoleSnapshot {
            roles: store.roles.clone(),
            members: store.members.clone(),
        }
    }

    /// The same, plus a synthetic row for `session_user` when the catalog has
    /// no such role — the trust-session identity [`RoleCatalog::login`] hands
    /// out. `pg_authid` has to show it: it is the role `current_user` names and
    /// the one `pg_stat_activity` reports, and a catalog that omitted it would
    /// contradict both.
    pub fn snapshot_with_session_role(&self, session_user: &str) -> RoleSnapshot {
        let mut snapshot = self.snapshot();
        if !snapshot.roles.iter().any(|r| r.name == session_user) {
            snapshot.roles.push(Role {
                oid: TRUST_SESSION_ROLE_OID,
                ..Role::bootstrap(session_user)
            });
        }
        snapshot
    }

    /// The bootstrap superuser's name: what every catalog row's owner OID
    /// resolves to.
    pub fn owner_name(&self) -> String {
        self.read()
            .roles
            .iter()
            .find(|r| r.oid == BOOTSTRAP_ROLE_OID)
            .map_or_else(|| "postgres".to_string(), |r| r.name.clone())
    }

    pub fn lookup(&self, name: &str) -> Option<Role> {
        self.read().roles.iter().find(|r| r.name == name).cloned()
    }

    /// Whether any role in the cluster has a password stored.
    ///
    /// The one question the trust fallback in [`RoleCatalog::login`] turns on:
    /// as long as the answer is no, nothing here can be authenticated against
    /// and refusing an unknown name would only refuse clients that work today.
    /// Once it is yes, that fallback would hand a superuser session to anybody
    /// who connects under a name the catalog does not have — which is every
    /// password in it bypassed by a typo.
    pub fn any_password_set(&self) -> bool {
        self.read().roles.iter().any(|r| r.password.is_some())
    }

    /// The role a connection authenticating as `name` runs as, and what the
    /// caller has to do about it.
    ///
    /// PostgreSQL answers a name it does not have with a FATAL `28000`, decided
    /// by `pg_hba.conf`. There is no such file here, so the answer is read off
    /// the catalog instead: see [`RoleCatalog::any_password_set`] for why a
    /// cluster with no passwords keeps trusting an unknown name and one with a
    /// password stops.
    pub fn login(&self, name: &str) -> Login {
        let store = self.read();
        match store.roles.iter().find(|r| r.name == name) {
            Some(role) => Login::Known(role.clone()),
            None if store.roles.iter().any(|r| r.password.is_some()) => Login::NoSuchRole,
            // A synthetic superuser the catalog relations show alongside the
            // stored roles (see [`TRUST_SESSION_ROLE_OID`]), so `current_user`
            // and `pg_stat_activity` still name something that exists.
            None => Login::Trusted(Role {
                oid: TRUST_SESSION_ROLE_OID,
                ..Role::bootstrap(name)
            }),
        }
    }

    /// Whether the role `oid` names is a superuser. The trust-session role is
    /// one: see [`RoleCatalog::login`].
    pub fn is_superuser(&self, oid: u32) -> bool {
        oid == TRUST_SESSION_ROLE_OID
            || self
                .read()
                .roles
                .iter()
                .any(|r| r.oid == oid && r.superuser)
    }

    /// Whether `member` may act as `role`: `SET ROLE` accepts a superuser
    /// switching to anything, and anyone else only to a role they are a member
    /// of (directly or transitively). Switching to oneself is always allowed.
    pub fn can_set_role(&self, member: u32, role: u32) -> bool {
        if member == role || self.is_superuser(member) {
            return true;
        }
        self.read().is_member_of(role, member)
    }

    /// `in_role` lists roles the new role becomes a member of; `member` and
    /// `admin` list roles that become members of *it*, the latter with
    /// `ADMIN OPTION`.
    pub fn create_role(
        &self,
        name: &str,
        opts: &RoleOptions,
        in_role: &[String],
        member: &[String],
        admin: &[String],
        grantor: u32,
    ) -> Result<(), PgError> {
        self.with_store(|store| {
            if store.position(name).is_some() {
                return Err(PgError::new(
                    sqlstate::DUPLICATE_OBJECT,
                    format!("role \"{name}\" already exists"),
                ));
            }
            // Resolve every named role before creating anything: a statement
            // that fails on its last `IN ROLE` must not leave the role behind.
            let in_role: Vec<u32> = in_role
                .iter()
                .map(|n| store.require(n))
                .collect::<Result<_, _>>()?;
            let member: Vec<u32> = member
                .iter()
                .map(|n| store.require(n))
                .collect::<Result<_, _>>()?;
            let admin: Vec<u32> = admin
                .iter()
                .map(|n| store.require(n))
                .collect::<Result<_, _>>()?;

            let oid = store.alloc_oid();
            let mut role = Role::new(oid, name.to_string());
            role.apply(opts);
            let inherit = role.inherit;
            store.roles.push(role);
            for parent in in_role {
                add_membership(store, parent, oid, grantor, false, inherit)?;
            }
            for child in member {
                let child_inherit = store
                    .roles
                    .iter()
                    .find(|r| r.oid == child)
                    .is_some_and(|r| r.inherit);
                add_membership(store, oid, child, grantor, false, child_inherit)?;
            }
            for child in admin {
                let child_inherit = store
                    .roles
                    .iter()
                    .find(|r| r.oid == child)
                    .is_some_and(|r| r.inherit);
                add_membership(store, oid, child, grantor, true, child_inherit)?;
            }
            Ok(())
        })
    }

    pub fn alter_role(&self, name: &str, opts: &RoleOptions) -> Result<(), PgError> {
        self.with_store(|store| {
            let index = store.position(name).ok_or_else(|| undefined_role(name))?;
            store.roles[index].apply(opts);
            Ok(())
        })
    }

    pub fn rename_role(&self, name: &str, new_name: &str) -> Result<(), PgError> {
        self.with_store(|store| {
            let index = store.position(name).ok_or_else(|| undefined_role(name))?;
            if name != new_name && store.position(new_name).is_some() {
                return Err(PgError::new(
                    sqlstate::DUPLICATE_OBJECT,
                    format!("role \"{new_name}\" already exists"),
                ));
            }
            store.roles[index].name = new_name.to_string();
            Ok(())
        })
    }

    /// The `ALTER ROLE … SET`/`RESET` family: a `key` of `None` is `RESET ALL`,
    /// and a named `key` with no `value` is `RESET <key>`.
    pub fn set_config(
        &self,
        name: &str,
        key: Option<&str>,
        value: Option<&str>,
    ) -> Result<(), PgError> {
        self.with_store(|store| {
            let index = store.position(name).ok_or_else(|| undefined_role(name))?;
            let config = &mut store.roles[index].config;
            let Some(key) = key else {
                config.clear();
                return Ok(());
            };
            let prefix = format!("{key}=");
            config.retain(|entry| !entry.starts_with(&prefix));
            if let Some(value) = value {
                config.push(format!("{prefix}{value}"));
            }
            Ok(())
        })
    }

    /// `session_user`/`current_user` are the OIDs the dropping session runs as:
    /// PostgreSQL refuses to drop either.
    pub fn drop_role(
        &self,
        name: &str,
        if_exists: bool,
        session_user: u32,
        current_user: u32,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        self.with_store(|store| {
            let Some(index) = store.position(name) else {
                if if_exists {
                    return Ok(vec![CatalogNotice::new(format!(
                        "role \"{name}\" does not exist, skipping"
                    ))]);
                }
                return Err(undefined_role(name));
            };
            let oid = store.roles[index].oid;
            if oid == current_user {
                return Err(PgError::new(
                    sqlstate::OBJECT_IN_USE,
                    "current user cannot be dropped",
                ));
            }
            if oid == session_user {
                return Err(PgError::new(
                    sqlstate::OBJECT_IN_USE,
                    "session user cannot be dropped",
                ));
            }
            // Every catalog row reports the bootstrap role as its owner, so
            // dropping it would leave `pg_get_userbyid(relowner)` resolving to
            // nothing. PostgreSQL refuses it too, and reaches this check in the
            // same order: dropping the bootstrap role *from its own session* is
            // reported as the current user, and only another session sees this.
            if oid == BOOTSTRAP_ROLE_OID {
                return Err(PgError::new(
                    sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!(
                        "cannot drop role {name} because it is required by the database system"
                    ),
                ));
            }
            store.roles.remove(index);
            // Memberships naming the role go with it: PostgreSQL leaves no
            // `pg_auth_members` row behind pointing at a dropped role, and a
            // row that outlived one would dangle in the catalog relations.
            store.members.retain(|m| m.role != oid && m.member != oid);
            Ok(Vec::new())
        })
    }

    /// The OID a `GRANTED BY <named>` clause records as the grantor, or the
    /// error PostgreSQL raises for a role that may not be named there.
    ///
    /// Two checks, in PostgreSQL's own order (18.4 reports whichever fails
    /// first): `current_user` must be able to act as the named role, and the
    /// named role must itself be entitled to hand out every role being granted.
    /// A superuser passes the first as anyone and the second as any role.
    pub fn resolve_grantor(
        &self,
        named: &str,
        current_user: u32,
        granted: &[String],
    ) -> Result<u32, PgError> {
        let store = self.read();
        let grantor = store.require(named)?;
        // Read off the guard already held rather than through
        // [`RoleCatalog::is_superuser`]: taking a second read lock while this
        // one is alive is what makes a writer waiting in between a deadlock.
        let is_superuser = |oid: u32| {
            oid == TRUST_SESSION_ROLE_OID || store.roles.iter().any(|r| r.oid == oid && r.superuser)
        };
        if grantor != current_user
            && !is_superuser(current_user)
            && !store.is_member_of(grantor, current_user)
        {
            return Err(PgError::new(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to grant privileges as role \"{named}\""),
            )
            .with_detail(format!(
                "Only roles with privileges of role \"{named}\" may grant privileges as this role."
            )));
        }
        let grantor_is_superuser = is_superuser(grantor);
        for role in granted {
            let role_oid = store.require(role)?;
            let has_admin = store
                .members
                .iter()
                .any(|m| m.role == role_oid && m.member == grantor && m.admin_option);
            if !grantor_is_superuser && !has_admin {
                return Err(PgError::new(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to grant privileges as role \"{named}\""),
                )
                .with_detail(format!(
                    "The grantor must have the ADMIN option on role \"{role}\"."
                )));
            }
        }
        Ok(grantor)
    }

    pub fn grant_membership(
        &self,
        roles: &[String],
        members: &[String],
        admin_option: bool,
        grantor: u32,
    ) -> Result<Vec<CatalogNotice>, PgError> {
        self.with_store(|store| {
            let roles: Vec<u32> = roles
                .iter()
                .map(|n| store.require(n))
                .collect::<Result<_, _>>()?;
            let members: Vec<u32> = members
                .iter()
                .map(|n| store.require(n))
                .collect::<Result<_, _>>()?;
            let mut notices = Vec::new();
            for role in roles {
                for &member in &members {
                    let inherit = store
                        .roles
                        .iter()
                        .find(|r| r.oid == member)
                        .is_some_and(|r| r.inherit);
                    if let Some(notice) =
                        add_membership(store, role, member, grantor, admin_option, inherit)?
                    {
                        notices.push(notice);
                    }
                }
            }
            Ok(notices)
        })
    }

    /// With `admin_option_for` the membership stays and only the option is
    /// dropped.
    ///
    /// A revoke only reaches the grant `grantor` made: PostgreSQL leaves a
    /// membership granted by somebody else in place — even for a superuser — and
    /// says so in a WARNING, which is what the returned messages carry.
    pub fn revoke_membership(
        &self,
        roles: &[String],
        members: &[String],
        admin_option_for: bool,
        grantor: u32,
    ) -> Result<Vec<String>, PgError> {
        self.with_store(|store| {
            let role_oids: Vec<u32> = roles
                .iter()
                .map(|n| store.require(n))
                .collect::<Result<_, _>>()?;
            let member_oids: Vec<u32> = members
                .iter()
                .map(|n| store.require(n))
                .collect::<Result<_, _>>()?;
            let mut warnings = Vec::new();
            for role in role_oids {
                for &member in &member_oids {
                    let found = store
                        .members
                        .iter_mut()
                        .find(|m| m.role == role && m.member == member && m.grantor == grantor);
                    let Some(edge) = found else {
                        warnings.push(format!(
                            "role \"{}\" has not been granted membership in role \"{}\" by role \"{}\"",
                            store.name_of(member),
                            store.name_of(role),
                            store.name_of(grantor),
                        ));
                        continue;
                    };
                    if admin_option_for {
                        edge.admin_option = false;
                    } else {
                        store.members.retain(|m| {
                            !(m.role == role && m.member == member && m.grantor == grantor)
                        });
                    }
                }
            }
            Ok(warnings)
        })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, RoleStore> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` against the catalog and persist the result, all under the write
    /// lock so no reader sees a store that disagrees with what is on disk.
    ///
    /// `f` mutates a *copy*, which is installed only once the file it produced
    /// is safely written. A DDL statement that fails — either in `f` or on the
    /// way to disk — therefore leaves the catalog exactly as it was, rather
    /// than in a state only this process knows about. Roles are few and the
    /// copy is shallow, so this costs a clone per DDL statement and nothing per
    /// read.
    fn with_store<T>(
        &self,
        f: impl FnOnce(&mut RoleStore) -> Result<T, PgError>,
    ) -> Result<T, PgError> {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let mut next = guard.clone();
        let value = f(&mut next)?;
        if let Some(path) = &self.path {
            write_atomically(path, &encode(&next)).map_err(|e| {
                PgError::new(
                    sqlstate::IO_ERROR,
                    format!("could not write the role catalog: {e}"),
                )
            })?;
        }
        *guard = next;
        Ok(value)
    }
}

/// `pg_authid`'s file, next to the relation catalog under the data directory.
/// Named for what it holds rather than after PostgreSQL's own file layout,
/// which this does not reproduce.
const ROLE_FILE: &str = "crabgresql_authid";

fn bootstrap_store(superuser: &str) -> RoleStore {
    RoleStore {
        roles: vec![Role::bootstrap(superuser)],
        members: Vec::new(),
        next_oid: FIRST_USER_OID,
    }
}

/// Write the role catalog a fresh cluster starts with: one superuser, and
/// nothing else. What `initdb` puts on disk before it stamps `PG_VERSION`.
///
/// `password` is plaintext and is hashed into a SCRAM verifier here — the
/// cleartext is never written anywhere. A file that already exists is left
/// alone: the roles in it are the cluster's, and `initdb` adopting a directory
/// written before this existed must not replace them.
pub fn write_bootstrap(data_dir: &Path, superuser: &str, password: Option<&str>) -> io::Result<()> {
    let path = data_dir.join(ROLE_FILE);
    if path.exists() {
        return Ok(());
    }
    let mut store = bootstrap_store(superuser);
    store.roles[0].password = password.map(scram::encrypt);
    write_atomically(&path, &encode(&store))
}

fn undefined_role(name: &str) -> PgError {
    PgError::new(
        sqlstate::UNDEFINED_OBJECT,
        format!("role \"{name}\" does not exist"),
    )
}

/// Store one membership edge, or report why it cannot exist. A repeat grant is
/// not an error in PostgreSQL: it updates the options in place.
fn add_membership(
    store: &mut RoleStore,
    role: u32,
    member: u32,
    grantor: u32,
    admin_option: bool,
    inherit_option: bool,
) -> Result<Option<CatalogNotice>, PgError> {
    if role == member {
        let name = store.name_of(role);
        return Err(PgError::new(
            sqlstate::INVALID_GRANT_OPERATION,
            format!("role \"{name}\" is a member of role \"{name}\""),
        ));
    }
    // A grant that would let a role reach itself through the membership graph
    // is the one PostgreSQL rejects outright: without this check `is_member_of`
    // would have a cycle to walk. The message states the membership that
    // already holds — `role` is reachable from `member`, so it *is* a member of
    // it — which is how 18.4 words it.
    if store.is_member_of(member, role) {
        return Err(PgError::new(
            sqlstate::INVALID_GRANT_OPERATION,
            format!(
                "role \"{}\" is a member of role \"{}\"",
                store.name_of(role),
                store.name_of(member)
            ),
        ));
    }
    if store
        .members
        .iter()
        .any(|m| m.role == role && m.member == member)
    {
        let notice = CatalogNotice::new(format!(
            "role \"{}\" has already been granted membership in role \"{}\" by role \"{}\"",
            store.name_of(member),
            store.name_of(role),
            store.name_of(grantor),
        ));
        let edge = store
            .members
            .iter_mut()
            .find(|m| m.role == role && m.member == member)
            .expect("checked just above");
        edge.admin_option |= admin_option;
        return Ok(Some(notice));
    }
    let oid = store.alloc_oid();
    store.members.push(Membership {
        oid,
        role,
        member,
        grantor,
        admin_option,
        inherit_option,
        set_option: true,
    });
    Ok(None)
}

// --- persistence -----------------------------------------------------------

/// Magic + version of the role file. Read strictly: unlike the relation
/// catalog's tails, this file is written whole on every change, so there is no
/// partial-read compatibility to keep — only a version to refuse.
const ROLE_MAGIC: &[u8; 4] = b"ROL1";
const ROLE_VERSION: u32 = 1;

fn encode(store: &RoleStore) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(ROLE_MAGIC);
    put_u32(&mut out, ROLE_VERSION);
    put_u32(&mut out, store.next_oid);
    put_u32(&mut out, store.roles.len() as u32);
    for role in &store.roles {
        put_u32(&mut out, role.oid);
        put_str(&mut out, &role.name);
        let flags = u8::from(role.superuser)
            | u8::from(role.inherit) << 1
            | u8::from(role.createrole) << 2
            | u8::from(role.createdb) << 3
            | u8::from(role.canlogin) << 4
            | u8::from(role.replication) << 5
            | u8::from(role.bypassrls) << 6;
        out.push(flags);
        out.extend_from_slice(&role.connlimit.to_le_bytes());
        match &role.password {
            Some(password) => {
                out.push(1);
                put_str(&mut out, password);
            }
            None => out.push(0),
        }
        match role.valid_until {
            Some(micros) => {
                out.push(1);
                out.extend_from_slice(&micros.to_le_bytes());
            }
            None => out.push(0),
        }
        put_u32(&mut out, role.config.len() as u32);
        for entry in &role.config {
            put_str(&mut out, entry);
        }
    }
    put_u32(&mut out, store.members.len() as u32);
    for member in &store.members {
        put_u32(&mut out, member.oid);
        put_u32(&mut out, member.role);
        put_u32(&mut out, member.member);
        put_u32(&mut out, member.grantor);
        out.push(
            u8::from(member.admin_option)
                | u8::from(member.inherit_option) << 1
                | u8::from(member.set_option) << 2,
        );
    }
    out
}

fn decode(bytes: &[u8]) -> io::Result<RoleStore> {
    let mut r = Reader { bytes, pos: 0 };
    if r.take(4)? != ROLE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "role catalog file has a bad magic number",
        ));
    }
    let version = r.u32()?;
    if version != ROLE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("role catalog file is version {version}, expected {ROLE_VERSION}"),
        ));
    }
    let next_oid = r.u32()?;
    let role_count = r.u32()?;
    let mut roles = Vec::with_capacity(role_count as usize);
    for _ in 0..role_count {
        let oid = r.u32()?;
        let name = r.string()?;
        let flags = r.u8()?;
        let connlimit = i32::from_le_bytes(r.take(4)?.try_into().expect("4 bytes"));
        let password = if r.u8()? == 1 {
            Some(r.string()?)
        } else {
            None
        };
        let valid_until = if r.u8()? == 1 {
            Some(i64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes")))
        } else {
            None
        };
        let config_count = r.u32()?;
        let mut config = Vec::with_capacity(config_count as usize);
        for _ in 0..config_count {
            config.push(r.string()?);
        }
        roles.push(Role {
            oid,
            name,
            superuser: flags & 1 != 0,
            inherit: flags & 2 != 0,
            createrole: flags & 4 != 0,
            createdb: flags & 8 != 0,
            canlogin: flags & 16 != 0,
            replication: flags & 32 != 0,
            bypassrls: flags & 64 != 0,
            connlimit,
            password,
            valid_until,
            config,
        });
    }
    let member_count = r.u32()?;
    let mut members = Vec::with_capacity(member_count as usize);
    for _ in 0..member_count {
        let oid = r.u32()?;
        let role = r.u32()?;
        let member = r.u32()?;
        let grantor = r.u32()?;
        let flags = r.u8()?;
        members.push(Membership {
            oid,
            role,
            member,
            grantor,
            admin_option: flags & 1 != 0,
            inherit_option: flags & 2 != 0,
            set_option: flags & 4 != 0,
        });
    }
    Ok(RoleStore {
        roles,
        members,
        next_oid,
    })
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.bytes.len());
        let Some(end) = end else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "role catalog file ended mid-record",
            ));
        };
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn string(&mut self) -> io::Result<String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }
}

/// Write `bytes` to `path` so a crash leaves either the old file or the new one:
/// a temporary file is written and fsynced, renamed over the target, and the
/// directory fsynced so the rename itself is durable.
///
/// The temporary file is owner-only from the moment it exists, and the rename
/// carries that mode to the target: this file holds password verifiers, and a
/// verifier is offline-attackable by anyone who can read it.
fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut file = crate::initdb::create_private(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        // A directory fsync is what makes the rename survive a crash. Not every
        // platform allows opening a directory for that; where it fails, the
        // rename is still atomic, just not yet durable.
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

// --- SCRAM verifiers -------------------------------------------------------

/// PostgreSQL's stored form of a password: the SCRAM-SHA-256 verifier
/// `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`, base64 as
/// RFC 5802 defines it.
///
/// This is what `pg_authid.rolpassword` shows, and what [`crate::auth`] checks
/// a client's SASL exchange against — the crypto lives here, the protocol lives
/// there.
pub mod scram {
    use sha2::{Digest, Sha256};

    /// PostgreSQL's default `password_encryption` iteration count.
    const ITERATIONS: u32 = 4096;
    const SALT_LEN: usize = 16;
    pub const PREFIX: &str = "SCRAM-SHA-256$";

    /// A stored verifier taken apart: everything the server side of SCRAM
    /// needs, and nothing that would let it compute a password.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Verifier {
        pub iterations: u32,
        pub salt: Vec<u8>,
        /// `SHA256(ClientKey)`. What a client's proof is checked against.
        pub stored_key: Vec<u8>,
        /// Signs the server's half of the exchange, proving to the client that
        /// we hold the verifier.
        pub server_key: Vec<u8>,
    }

    impl Verifier {
        /// Read `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`.
        /// `None` for anything that is not one — an `md5…` password, or a
        /// verifier this build cannot read.
        pub fn parse(stored: &str) -> Option<Verifier> {
            let rest = stored.strip_prefix(PREFIX)?;
            let (params, keys) = rest.split_once('$')?;
            let (iterations, salt) = params.split_once(':')?;
            let (stored_key, server_key) = keys.split_once(':')?;
            let b64 = |s: &str| crabgresql_types::text::decode(s, "base64").ok();
            Some(Verifier {
                iterations: iterations.parse().ok()?,
                salt: b64(salt)?,
                // A key of the wrong length would make `verify` compare against
                // a truncated digest, which is a weaker check rather than a
                // failing one — so it is refused here, where it is still a
                // parse question.
                stored_key: b64(stored_key).filter(|k| k.len() == 32)?,
                server_key: b64(server_key).filter(|k| k.len() == 32)?,
            })
        }
    }

    /// Hash `password` into a verifier, unless it already is one: a client may
    /// send a pre-computed `SCRAM-SHA-256$…` or legacy `md5…` string, and
    /// PostgreSQL stores those verbatim rather than hashing the hash.
    pub fn encrypt(password: &str) -> String {
        if is_verifier(password) {
            return password.to_string();
        }
        let mut salt = [0u8; SALT_LEN];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut salt);
        build(password, &salt, ITERATIONS)
    }

    /// Whether the string is already a stored verifier rather than a plaintext
    /// password.
    ///
    /// A SCRAM verifier has to *parse*, not merely start with the right
    /// letters. Storing an unparseable `SCRAM-SHA-256$…` verbatim would leave a
    /// role whose password can never be checked — [`Verifier::parse`] returns
    /// `None` at login and the connection is refused forever, including for the
    /// bootstrap superuser. PostgreSQL parses it here too, and hashes what does
    /// not parse as the plaintext it evidently is.
    fn is_verifier(password: &str) -> bool {
        Verifier::parse(password).is_some() || is_md5(password)
    }

    /// A legacy `md5<32 hex digits>` hash. The length check is what tells it
    /// from a plaintext password that merely starts with those three letters.
    fn is_md5(password: &str) -> bool {
        password.len() == 35
            && password.starts_with("md5")
            && password[3..].bytes().all(|b| b.is_ascii_hexdigit())
    }

    fn build(password: &str, salt: &[u8], iterations: u32) -> String {
        let salted = pbkdf2_sha256(&prepare(password), salt, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = Sha256::digest(client_key).to_vec();
        let server_key = hmac_sha256(&salted, b"Server Key");
        format!(
            "SCRAM-SHA-256${iterations}:{}${}:{}",
            base64(salt),
            base64(&stored_key),
            base64(&server_key),
        )
    }

    /// The bytes a password is hashed as: SASLprep (RFC 4013), which RFC 7677
    /// requires of both sides of the exchange.
    ///
    /// Not an optional nicety — the *client* preps the password before it
    /// computes its proof, so a server that skipped this would produce a
    /// verifier no client could ever match. With `--pwfile` that locks the only
    /// role a fresh cluster has out of it.
    ///
    /// A password SASLprep refuses (invalid UTF-8, a prohibited character) is
    /// hashed as it arrived rather than rejected. That is what PostgreSQL does,
    /// and what the drivers do, so the two halves still agree; refusing would
    /// instead make a password unusable that a client is perfectly able to send.
    pub fn prepare(password: &str) -> Vec<u8> {
        match stringprep::saslprep(password) {
            Ok(prepared) => prepared.into_owned().into_bytes(),
            Err(_) => password.as_bytes().to_vec(),
        }
    }

    /// PBKDF2 with HMAC-SHA-256 and a 32-byte output, which is exactly one
    /// block: the outer loop RFC 8018 defines collapses to a single `U`
    /// sequence, so this is the whole algorithm rather than a special case of it.
    fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
        let mut block = salt.to_vec();
        block.extend_from_slice(&1u32.to_be_bytes());
        let mut u = hmac_sha256(password, &block);
        let mut out = u;
        for _ in 1..iterations {
            u = hmac_sha256(password, &u);
            for (acc, byte) in out.iter_mut().zip(u.iter()) {
                *acc ^= byte;
            }
        }
        out
    }

    /// HMAC-SHA-256 (RFC 2104). SHA-256's block size is 64 bytes.
    pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
        const BLOCK: usize = 64;
        let mut padded = [0u8; BLOCK];
        if key.len() > BLOCK {
            padded[..32].copy_from_slice(&Sha256::digest(key));
        } else {
            padded[..key.len()].copy_from_slice(key);
        }
        let mut inner = Sha256::new();
        inner.update(padded.map(|b| b ^ 0x36));
        inner.update(message);
        let inner = inner.finalize();
        let mut outer = Sha256::new();
        outer.update(padded.map(|b| b ^ 0x5c));
        outer.update(inner);
        outer.finalize().into()
    }

    /// Unwrapped base64. `crabgresql_types`' own encoder is `encode(bytea,
    /// 'base64')`, which wraps at 76 columns because PostgreSQL's does — a
    /// verifier must not carry newlines.
    pub fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The verifier for a known password, salt and iteration count, checked
        /// against an independent PBKDF2-HMAC-SHA-256 implementation
        /// (Python's `hashlib`). The salt is fixed here because PostgreSQL
        /// generates a random one, so the *value* cannot be read off a live
        /// server — its shape can, and this matches what 18.4 stores in
        /// `pg_authid.rolpassword` after `CREATE ROLE r PASSWORD 'secret'`.
        #[test]
        fn verifier_matches_an_independent_pbkdf2() {
            let verifier = build("secret", b"0123456789abcdef", 4096);
            assert_eq!(
                verifier,
                "SCRAM-SHA-256$4096:MDEyMzQ1Njc4OWFiY2RlZg==$\
                 bpSY5Ze9NUH+I35LC3gVq+DpBfK46iXBxvhAKqVu9pE=:\
                 VpYlBuxyzeCI1KnctrefdljpB1mk3Gp7sBI/t11+NkQ="
            );
        }

        /// RFC 4231 test case 1, so the HMAC this builds PBKDF2 on is the real
        /// one rather than a lookalike.
        #[test]
        fn hmac_matches_rfc4231() {
            let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
            assert_eq!(
                crabgresql_types::hex::encode(&mac),
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
            );
        }

        /// An already-hashed password is stored as it arrived; a plaintext one
        /// that merely looks like it starts with `md5` is not.
        #[test]
        fn pre_hashed_passwords_are_kept_verbatim() {
            let md5 = format!("md5{}", "0".repeat(32));
            assert_eq!(encrypt(&md5), md5);
            let scram = build("secret", b"0123456789abcdef", 4096);
            assert_eq!(encrypt(&scram), scram);
            assert!(encrypt("md5secret").starts_with("SCRAM-SHA-256$"));
        }

        /// A password that *looks* like a verifier but is not one is a
        /// password, and gets hashed.
        ///
        /// Storing it verbatim would leave a role nothing can authenticate:
        /// the login path parses the stored string, gets `None`, and refuses
        /// the connection — every time, forever, with no way back in if it is
        /// the only superuser. PostgreSQL parses it here for the same reason.
        #[test]
        fn a_scram_shaped_string_that_does_not_parse_is_a_password() {
            for impostor in [
                "SCRAM-SHA-256$",
                "SCRAM-SHA-256$4096:not-base64",
                // Parses as far as its shape goes, but the keys are the wrong
                // length — a truncated digest is a weaker check, not a failing
                // one, so `Verifier::parse` refuses it and so must this.
                "SCRAM-SHA-256$4096:c2FsdA==$c2hvcnQ=:c2hvcnQ=",
                // A plausible password that happens to start this way.
                "SCRAM-SHA-256$ecret",
            ] {
                let stored = encrypt(impostor);
                assert_ne!(stored, impostor, "{impostor:?} should have been hashed");
                assert!(
                    Verifier::parse(&stored).is_some(),
                    "{impostor:?} produced a verifier that does not parse: {stored}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> RoleCatalog {
        RoleCatalog::in_memory("postgres")
    }

    #[test]
    fn create_and_drop_round_trip() {
        let cat = catalog();
        cat.create_role("alice", &RoleOptions::default(), &[], &[], &[], 10)
            .expect("create");
        assert!(cat.lookup("alice").is_some());
        let err = cat
            .create_role("alice", &RoleOptions::default(), &[], &[], &[], 10)
            .expect_err("duplicate");
        assert_eq!(err.code, sqlstate::DUPLICATE_OBJECT);
        cat.drop_role("alice", false, 10, 10).expect("drop");
        assert!(cat.lookup("alice").is_none());
        let err = cat.drop_role("alice", false, 10, 10).expect_err("missing");
        assert_eq!(err.code, sqlstate::UNDEFINED_OBJECT);
        let notices = cat.drop_role("alice", true, 10, 10).expect("if exists");
        assert_eq!(notices.len(), 1);
    }

    /// The session's own roles are the two PostgreSQL refuses to drop, and it
    /// distinguishes them: `current user` when the session has `SET ROLE`d to
    /// the target, `session user` when it logged in as it.
    #[test]
    fn dropping_the_sessions_own_role_is_refused() {
        let cat = catalog();
        cat.create_role("alice", &RoleOptions::default(), &[], &[], &[], 10)
            .expect("create");
        let alice = cat.lookup("alice").expect("alice").oid;
        let err = cat
            .drop_role("alice", false, 10, alice)
            .expect_err("current");
        assert_eq!(err.code, sqlstate::OBJECT_IN_USE);
        assert_eq!(err.message, "current user cannot be dropped");
        let err = cat
            .drop_role("alice", false, alice, 10)
            .expect_err("session");
        assert_eq!(err.message, "session user cannot be dropped");
    }

    #[test]
    fn membership_is_transitive_and_acyclic() {
        let cat = catalog();
        for name in ["alice", "devs", "staff"] {
            cat.create_role(name, &RoleOptions::default(), &[], &[], &[], 10)
                .expect("create");
        }
        cat.grant_membership(&["devs".into()], &["alice".into()], false, 10)
            .expect("grant");
        cat.grant_membership(&["staff".into()], &["devs".into()], false, 10)
            .expect("grant");
        let alice = cat.lookup("alice").expect("alice").oid;
        let staff = cat.lookup("staff").expect("staff").oid;
        assert!(cat.can_set_role(alice, staff));
        // Closing the loop is the grant PostgreSQL rejects.
        let err = cat
            .grant_membership(&["alice".into()], &["staff".into()], false, 10)
            .expect_err("cycle");
        assert_eq!(err.code, sqlstate::INVALID_GRANT_OPERATION);
        cat.revoke_membership(&["devs".into()], &["alice".into()], false, 10)
            .expect("revoke");
        assert!(!cat.can_set_role(alice, staff));
    }

    /// Dropping a role takes its memberships with it: a `pg_auth_members` row
    /// naming an OID `pg_authid` no longer has would be a dangling reference in
    /// the catalog relations.
    #[test]
    fn dropping_a_role_removes_its_memberships() {
        let cat = catalog();
        cat.create_role("alice", &RoleOptions::default(), &[], &[], &[], 10)
            .expect("create");
        cat.create_role("devs", &RoleOptions::default(), &[], &[], &[], 10)
            .expect("create");
        cat.grant_membership(&["devs".into()], &["alice".into()], false, 10)
            .expect("grant");
        cat.drop_role("devs", false, 10, 10).expect("drop");
        assert!(cat.snapshot().members.is_empty());
    }

    #[test]
    fn config_entries_are_replaced_by_key() {
        let cat = catalog();
        cat.create_role("alice", &RoleOptions::default(), &[], &[], &[], 10)
            .expect("create");
        cat.set_config("alice", Some("timezone"), Some("UTC"))
            .expect("set");
        cat.set_config("alice", Some("timezone"), Some("PST8PDT"))
            .expect("set");
        cat.set_config("alice", Some("extra_float_digits"), Some("2"))
            .expect("set");
        assert_eq!(
            cat.lookup("alice").expect("alice").config,
            vec![
                "timezone=PST8PDT".to_string(),
                "extra_float_digits=2".into()
            ]
        );
        cat.set_config("alice", Some("timezone"), None)
            .expect("reset");
        assert_eq!(
            cat.lookup("alice").expect("alice").config,
            vec!["extra_float_digits=2".to_string()]
        );
        cat.set_config("alice", None, None).expect("reset all");
        assert!(cat.lookup("alice").expect("alice").config.is_empty());
    }

    /// The whole store survives a round trip through the on-disk form — the
    /// property the file exists for.
    #[test]
    fn store_round_trips_through_the_file_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let cat = RoleCatalog::open(dir.path(), "postgres").expect("open");
            let opts = RoleOptions {
                canlogin: Some(true),
                connlimit: Some(5),
                password: Some(Some(scram::encrypt("secret"))),
                valid_until: Some(1_234_567_890),
                ..RoleOptions::default()
            };
            cat.create_role("alice", &opts, &[], &[], &[], 10)
                .expect("create");
            cat.create_role("devs", &RoleOptions::default(), &[], &[], &[], 10)
                .expect("create");
            cat.grant_membership(&["devs".into()], &["alice".into()], true, 10)
                .expect("grant");
            cat.set_config("alice", Some("timezone"), Some("UTC"))
                .expect("set");
        }
        let reopened = RoleCatalog::open(dir.path(), "ignored").expect("reopen");
        let alice = reopened.lookup("alice").expect("alice");
        assert!(alice.canlogin);
        assert_eq!(alice.connlimit, 5);
        assert_eq!(alice.valid_until, Some(1_234_567_890));
        assert_eq!(alice.config, vec!["timezone=UTC".to_string()]);
        assert!(
            alice
                .password
                .expect("password")
                .starts_with("SCRAM-SHA-256$")
        );
        // The bootstrap name is only used when the file is *created*: reopening
        // must not rename the superuser out from under the stored roles.
        assert!(reopened.lookup("postgres").is_some());
        let members = reopened.snapshot().members;
        assert_eq!(members.len(), 1);
        assert!(members[0].admin_option);
    }
}
