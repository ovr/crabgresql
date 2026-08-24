//! `pg_has_role` and the `has_*_privilege` families.
//!
//! Clean-room (see AGENTS.md): every value, message and SQLSTATE below was
//! probed against PostgreSQL 18.4.
//!
//! # The model these functions report
//!
//! There is no object-level `GRANT` in this build — the statement is refused
//! with `0A000` — so no object carries an ACL, and every one of them is owned by
//! the bootstrap superuser (see `crabgresql_catalog::BOOTSTRAP_ROLE_OID`). That
//! is not a gap these functions paper over: it *is* the state of the cluster,
//! and PostgreSQL answers exactly the same questions about a cluster whose ACLs
//! are all default. So the answer is decided by three facts and nothing else:
//!
//! 1. a superuser holds every privilege on everything;
//! 2. so does the owner, and so does any role that inherits the owner's
//!    privileges — including the grant option, which is the owner's to give;
//! 3. otherwise only what upstream leaves to PUBLIC: `USAGE` on a type,
//!    `EXECUTE` on a function, and the grants `initdb` writes out — `SELECT` on
//!    a system catalog (but not on the eight it deliberately closes) and `USAGE`
//!    on the `pg_catalog` and `public` schemas. PUBLIC never holds a grant
//!    option, and never holds `CREATE` on `public`: since PostgreSQL 15 that
//!    one stayed with the owner.
//!
//! Which objects PUBLIC reaches is the catalog's to report — `ObjectAcl`'s
//! `granted_to_public` — because the answer is a property of the object, not of
//! the question. A user table, a `CREATE SCHEMA` namespace and a sequence carry
//! no such grant, so they answer `false` for anyone but the owner and the
//! superusers, which is what PostgreSQL answers for the same objects.
//!
//! # Resolving the arguments
//!
//! Each family reads its object name with the input function upstream hands it,
//! and they do not agree — which is observable, so it is reproduced rather than
//! unified:
//!
//! * a **relation** (table, sequence, column, any-column) is a qualified
//!   identifier: case-folded, quotable, and never an OID in disguise. See
//!   [`crate::reg::relation_oid_from_name`].
//! * a **schema** and a **role** are stored names taken verbatim — no parsing,
//!   no case folding: `has_schema_privilege('PUBLIC','USAGE')` is an error.
//! * a **type** and a **function** go through `regtypein` / `regprocedurein`,
//!   which do read all-digits as an OID and `-` as `InvalidOid` (reported here
//!   as "does not exist").

use crabgresql_binder::{AclClass, ArgForm, PrivCall};
use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{RegKind, Value};

use crate::{CatalogOps, ExecError, ObjectAcl, RoleMembership};

/// The privileges one object class recognizes, in the spelling the argument
/// string uses. A set rather than a bitmask: nothing here can hold a *subset*
/// of them, so the only question a parsed privilege has to answer is whether the
/// class recognizes it and whether the grant option was asked for.
fn privilege_names(class: AclClass) -> &'static [&'static str] {
    match class {
        // `MAINTAIN` is PostgreSQL 17's addition and is listed here for the same
        // reason the rest are: the parity target is a current server, and the
        // keyword is recognized whether or not anything acts on it.
        AclClass::Relation => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
            "MAINTAIN",
        ],
        // A column carries only the four privileges that can be granted on one.
        AclClass::Column | AclClass::AnyColumn => &["SELECT", "INSERT", "UPDATE", "REFERENCES"],
        AclClass::Sequence => &["SELECT", "UPDATE", "USAGE"],
        AclClass::Schema => &["CREATE", "USAGE"],
        AclClass::Type | AclClass::Server | AclClass::ForeignDataWrapper => &["USAGE"],
        AclClass::Function => &["EXECUTE"],
        AclClass::Role => &["USAGE", "MEMBER", "SET"],
    }
}

/// One privilege the argument string asked about.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Privilege {
    /// Index into [`privilege_names`] for the class — which is all the identity
    /// a privilege needs here.
    name: &'static str,
    /// Whether the call asked for the grant option rather than the privilege
    /// itself.
    grant_option: bool,
}

/// Parse the privilege argument: a comma-separated list, each entry optionally
/// carrying a grant-option suffix.
///
/// The whole list is true if **any** entry is held, which is upstream's rule and
/// the reason `information_schema`'s filters can spell seven privileges in one
/// call. Entries are trimmed and matched case-insensitively; the suffix must be
/// exactly one space-separated `WITH GRANT OPTION` (`pg_has_role` takes
/// `WITH ADMIN OPTION` for the same thing), so `'SELECT WITH  GRANT OPTION'` is
/// as unrecognized as `'BOGUS'` — probed.
fn parse_privileges(class: AclClass, spec: &str) -> Result<Vec<Privilege>, ExecError> {
    spec.split(',')
        .map(|entry| {
            let entry = entry.trim();
            let (name, grant_option) = strip_grant_option(class, entry);
            privilege_names(class)
                .iter()
                .find(|known| known.eq_ignore_ascii_case(name))
                .map(|known| Privilege {
                    name: known,
                    grant_option,
                })
                .ok_or_else(|| {
                    ExecError::new(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format!("unrecognized privilege type: \"{entry}\""),
                    )
                })
        })
        .collect()
}

/// Split a trimmed entry into its privilege name and whether the grant option
/// was asked for. A role's membership is granted `WITH ADMIN OPTION`, so
/// `pg_has_role` accepts that spelling as well — and accepts the `GRANT` one
/// too, which upstream's table lists for both.
fn strip_grant_option(class: AclClass, entry: &str) -> (&str, bool) {
    for suffix in [" WITH GRANT OPTION", " WITH ADMIN OPTION"] {
        if suffix == " WITH ADMIN OPTION" && class != AclClass::Role {
            continue;
        }
        if entry.len() > suffix.len() {
            let (head, tail) = entry.split_at(entry.len() - suffix.len());
            if tail.eq_ignore_ascii_case(suffix) {
                return (head, true);
            }
        }
    }
    (entry, false)
}

/// Evaluate one call. `args` arrive in the order [`PrivCall`] describes: the
/// optional role, the object, the optional column, and the privilege string.
///
/// The steps run in PostgreSQL's order, which is observable in which error a
/// call with two problems reports: the role first, then the object, then the
/// column, then the privilege string — except that a sequence's *kind* is
/// checked after the string, so `has_sequence_privilege('pg_class','BOGUS')`
/// complains about the privilege and `(…,'SELECT')` about the relation kind.
pub fn eval_has_privilege(
    call: PrivCall,
    args: &[Value],
    ops: &dyn CatalogOps,
) -> Result<Value, ExecError> {
    // Every one of the 66 functions is STRICT.
    if args.iter().any(|arg| matches!(arg, Value::Null)) {
        return Ok(Value::Null);
    }
    let mut next = args.iter();
    let role = match call.user {
        // A role OID is taken as it stands: upstream checks no existence here,
        // and an OID no role answers to is simply a role with no privileges.
        Some(form) => resolve_role(form, next.next().expect("role argument"), ops)?,
        None => ops.current_user_oid(),
    };
    let object = next.next().expect("object argument");
    let column = call.column.map(|_| next.next().expect("column argument"));
    let privileges = next.next().expect("privilege argument");
    let oid = resolve_object(call.class, call.object, object, ops)?;
    if call.class == AclClass::Role {
        // A role is not an object with an owner: the question is about the
        // membership itself, so it is answered from the three facts
        // `pg_has_role` distinguishes. An OID no role answers to is not an
        // error here — it is simply a role this one is not a member of.
        let membership = ops.role_membership(role, oid);
        return Ok(Value::Bool(
            parse_privileges(call.class, text(privileges))?
                .iter()
                .any(|privilege| holds_role(*privilege, membership)),
        ));
    }
    let Some(acl) = ops.object_acl(call.class, oid) else {
        // No such object: NULL. A superuser sees `true` instead for the classes
        // whose existence check upstream folds into the ACL lookup — its
        // superuser short-circuit runs first there. The relation families are
        // the exception: they read the relation's row before anything else, so a
        // missing relation is NULL for a superuser too. Probed both ways.
        return Ok(match relation_class(call.class) {
            true => Value::Null,
            false => match ops.role_membership(role, role).superuser {
                true => Value::Bool(true),
                false => Value::Null,
            },
        });
    };
    // The column is resolved before the privilege string, as upstream resolves
    // it — a missing column beats an unrecognized privilege.
    if let (Some(form), Some(value)) = (call.column, column)
        && !column_exists(form, value, oid, ops)?
    {
        return Ok(Value::Null);
    }
    let privileges = parse_privileges(call.class, text(privileges))?;
    if call.class == AclClass::Sequence && !acl.is_sequence {
        let name = ops
            .rel_name(oid)
            .map_or_else(|| oid.to_string(), |(_, name)| name);
        return Err(ExecError::new(
            sqlstate::WRONG_OBJECT_TYPE,
            format!("\"{name}\" is not a sequence"),
        ));
    }
    let owner = ops.role_membership(role, acl.owner);
    Ok(Value::Bool(privileges.iter().any(|privilege| {
        holds_object(call.class, *privilege, owner, acl)
    })))
}

/// Whether the class asks about a relation. Those four read the relation's row
/// before anything else, which is why an OID naming no relation is NULL for them
/// even when the asking role is a superuser.
fn relation_class(class: AclClass) -> bool {
    matches!(
        class,
        AclClass::Relation | AclClass::Column | AclClass::AnyColumn | AclClass::Sequence
    )
}

/// Whether the role holds one privilege over another role.
fn holds_role(privilege: Privilege, membership: RoleMembership) -> bool {
    if privilege.grant_option {
        return membership.admin;
    }
    match privilege.name {
        "USAGE" => membership.inherits,
        "MEMBER" => membership.member,
        _ => membership.set,
    }
}

/// Whether the role holds one privilege over one object, under the default-ACL
/// model this module documents.
fn holds_object(
    class: AclClass,
    privilege: Privilege,
    owner: RoleMembership,
    acl: ObjectAcl,
) -> bool {
    // The owner holds everything on it, grant option included, and so does a
    // role that inherits the owner's privileges. `inherits` rather than
    // `member`: privileges that need a `SET ROLE` to arrive are not held.
    if owner.superuser || owner.inherits {
        return true;
    }
    if privilege.grant_option {
        // Nothing below is granted `WITH GRANT OPTION` by a default ACL.
        return false;
    }
    // What is left is PUBLIC's, and which privilege that is depends on the
    // class; whether *this* object carries the grant at all is the catalog's
    // answer (`granted_to_public`), which is how a closed system catalog and an
    // open one part company.
    match class {
        AclClass::Type => acl.granted_to_public && privilege.name == "USAGE",
        AclClass::Function => acl.granted_to_public && privilege.name == "EXECUTE",
        // USAGE only: `CREATE` on `public` belongs to the owner, and has since
        // PostgreSQL 15.
        AclClass::Schema => acl.granted_to_public && privilege.name == "USAGE",
        AclClass::Relation | AclClass::Column | AclClass::AnyColumn | AclClass::Sequence => {
            acl.granted_to_public && privilege.name == "SELECT"
        }
        // A foreign server and a foreign-data wrapper grant nothing to PUBLIC,
        // and neither exists in this build to be asked about.
        AclClass::Server | AclClass::ForeignDataWrapper | AclClass::Role => false,
    }
}

/// The role argument: a stored name matched exactly, or an OID as it stands.
fn resolve_role(form: ArgForm, value: &Value, ops: &dyn CatalogOps) -> Result<u32, ExecError> {
    match form {
        ArgForm::Oid => Ok(oid_of(value)),
        _ => {
            let name = text(value);
            ops.role_oid(name).ok_or_else(|| {
                ExecError::new(
                    sqlstate::UNDEFINED_OBJECT,
                    format!("role \"{name}\" does not exist"),
                )
            })
        }
    }
}

/// The object argument. An OID is taken as it stands — whether anything answers
/// to it is the caller's question, and the answer to it is a NULL rather than an
/// error — while a *name* that names nothing is the class's own error.
fn resolve_object(
    class: AclClass,
    form: ArgForm,
    value: &Value,
    ops: &dyn CatalogOps,
) -> Result<u32, ExecError> {
    if form == ArgForm::Oid {
        return Ok(oid_of(value));
    }
    let name = text(value);
    let oid = match class {
        AclClass::Relation | AclClass::Column | AclClass::AnyColumn | AclClass::Sequence => {
            crate::reg::relation_oid_from_name(name, ops)?
        }
        AclClass::Schema => ops.namespace_oid(name).ok_or_else(|| {
            ExecError::new(
                sqlstate::INVALID_SCHEMA_NAME,
                format!("schema \"{name}\" does not exist"),
            )
        })?,
        AclClass::Role => ops.role_oid(name).ok_or_else(|| {
            ExecError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("role \"{name}\" does not exist"),
            )
        })?,
        // `regtypein`/`regprocedurein` raise for a name they cannot read and
        // answer `InvalidOid` for `-`, which upstream turns into the same
        // "does not exist" a miss gets.
        AclClass::Type => nonzero(
            crate::reg::from_text(RegKind::Type, name, ops)?.oid,
            sqlstate::UNDEFINED_OBJECT,
            format!("type \"{name}\" does not exist"),
        )?,
        AclClass::Function => nonzero(
            crate::reg::from_text(RegKind::Procedure, name, ops)?.oid,
            sqlstate::UNDEFINED_FUNCTION,
            format!("function \"{name}\" does not exist"),
        )?,
        // Neither catalog has a row in this build, so every name misses. Written
        // as a lookup rather than as a constant error so the day one exists this
        // answers for it.
        //
        // TODO: resolve against `pg_foreign_server` / `pg_foreign_data_wrapper`
        // once `CREATE SERVER` exists.
        AclClass::Server => {
            return Err(ExecError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("server \"{name}\" does not exist"),
            ));
        }
        AclClass::ForeignDataWrapper => {
            return Err(ExecError::new(
                sqlstate::UNDEFINED_OBJECT,
                format!("foreign-data wrapper \"{name}\" does not exist"),
            ));
        }
    };
    Ok(oid)
}

/// Whether the column argument names a column of `relation`. A *name* that does
/// not is an error; a *number* that does not is NULL, which is what the `false`
/// here becomes.
fn column_exists(
    form: ArgForm,
    value: &Value,
    relation: u32,
    ops: &dyn CatalogOps,
) -> Result<bool, ExecError> {
    if form == ArgForm::Attnum {
        let Value::Int2(attnum) = value else {
            return Ok(false);
        };
        return Ok(ops.has_attribute(relation, *attnum));
    }
    let name = text(value);
    if ops.attribute_number(relation, name).is_some() {
        return Ok(true);
    }
    let relation = ops
        .rel_name(relation)
        .map_or_else(|| relation.to_string(), |(_, name)| name);
    Err(ExecError::new(
        sqlstate::UNDEFINED_COLUMN,
        format!("column \"{name}\" of relation \"{relation}\" does not exist"),
    ))
}

/// An OID that must not be `InvalidOid`, with the miss the class reports.
fn nonzero(oid: u32, state: &'static str, message: String) -> Result<u32, ExecError> {
    match oid {
        0 => Err(ExecError::new(state, message)),
        oid => Ok(oid),
    }
}

/// The text of an argument the binder declared `text` or `name`. Both arrive as
/// [`Value::Text`]; anything else cannot reach here.
fn text(value: &Value) -> &str {
    match value {
        Value::Text(s) => s,
        _ => "",
    }
}

/// The OID of an argument the binder declared `oid`, which the coercion has
/// already made one.
fn oid_of(value: &Value) -> u32 {
    crabgresql_types::compare::oid_of(value)
}
