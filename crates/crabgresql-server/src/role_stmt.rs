//! The role statements: `CREATE`/`ALTER`/`DROP ROLE`, `GRANT`/`REVOKE` of role
//! membership, and `SET ROLE` / `SET SESSION AUTHORIZATION`.
//!
//! This is the AST-facing half of [`crate::roles`]: it reads what the grammar
//! produced, resolves it to the catalog's own vocabulary, and reports the errors
//! PostgreSQL reports for a statement that cannot be carried out. Everything
//! about *storing* a role lives on the other side of [`RoleCatalog`].
//!
//! Privileges on objects are not modelled at all, so a `GRANT` that names one
//! (`GRANT SELECT ON t TO r`) is still refused as unsupported rather than
//! silently accepted — see [`execute_grant`].

use crabgresql_parser::ast;
use crabgresql_pg_wire::sqlstate;

use crate::error::PgError;
use crate::query::{Notice, QueryResult, normalize_ident, single_object_name, to_notices};
use crate::roles::{RoleOptions, scram};
use crate::session::Session;

pub(crate) fn execute_create_role(
    session: &mut Session,
    create: &ast::CreateRole,
) -> Result<QueryResult, PgError> {
    // PostgreSQL's grammar has no `IF NOT EXISTS` for this statement; the
    // vendored parser accepts one, so it is refused here to keep the surface
    // the same shape.
    if create.if_not_exists {
        return Err(PgError::syntax("syntax error at or near \"NOT\""));
    }
    if create.authorization_owner.is_some() {
        return Err(PgError::feature_not_supported(
            "CREATE ROLE ... AUTHORIZATION is not supported yet",
        ));
    }
    let opts = role_options(create, session)?;
    let in_role = idents(&create.in_role)
        .into_iter()
        .chain(idents(&create.in_group))
        .collect::<Vec<_>>();
    let member = idents(&create.role)
        .into_iter()
        .chain(idents(&create.user))
        .collect::<Vec<_>>();
    let admin = idents(&create.admin);
    // `IN ROLE`/`ROLE`/`ADMIN` are memberships like any other, so they record a
    // grantor the same way a `GRANT` does.
    let grantor = grantor(session, None, &[])?;
    for name in &create.names {
        let name = single_object_name(name, "role")?;
        session
            .roles
            .create_role(&name, &opts, &in_role, &member, &admin, grantor)?;
    }
    Ok(QueryResult::command("CREATE ROLE"))
}

pub(crate) fn execute_alter_role(
    session: &mut Session,
    name: &ast::Ident,
    operation: &ast::AlterRoleOperation,
) -> Result<QueryResult, PgError> {
    let name = normalize_ident(name);
    match operation {
        ast::AlterRoleOperation::WithOptions { options } => {
            let mut opts = RoleOptions::default();
            for option in options {
                apply_role_option(&mut opts, option, session)?;
            }
            session.roles.alter_role(&name, &opts)?;
        }
        ast::AlterRoleOperation::RenameRole { role_name } => {
            let new_name = normalize_ident(role_name);
            session.roles.rename_role(&name, &new_name)?;
            // A rename leaves the session naming a role that no longer exists,
            // so the identity it carries has to follow the role it points at.
            if session.user == name {
                session.user = new_name.clone();
            }
            if let Some((current, _)) = &mut session.current_role
                && *current == name
            {
                *current = new_name.clone();
            }
            if session.auth_user == name {
                session.auth_user = new_name;
            }
        }
        ast::AlterRoleOperation::Set {
            config_name,
            config_value,
            in_database,
        } => {
            if in_database.is_some() {
                return Err(PgError::feature_not_supported(
                    "ALTER ROLE ... IN DATABASE is not supported yet",
                ));
            }
            let key = config_name.to_string();
            // Stored under the parameter's canonical spelling, which is what
            // `rolconfig` shows: PostgreSQL records `TimeZone=UTC` for
            // `ALTER ROLE r SET timezone = 'UTC'`.
            let Some(def) = crate::guc::lookup(&key) else {
                return Err(PgError::new(
                    sqlstate::UNDEFINED_OBJECT,
                    format!("unrecognized configuration parameter \"{key}\""),
                ));
            };
            match config_value {
                ast::SetConfigValue::Default => {
                    session.roles.set_config(&name, Some(def.name), None)?
                }
                ast::SetConfigValue::FromCurrent => {
                    let value = (def.show)(session);
                    session
                        .roles
                        .set_config(&name, Some(def.name), Some(&value))?;
                }
                ast::SetConfigValue::Value(expr) => {
                    let value = config_literal(expr)?;
                    session
                        .roles
                        .set_config(&name, Some(def.name), Some(&value))?;
                }
            }
        }
        ast::AlterRoleOperation::Reset {
            config_name,
            in_database,
        } => {
            if in_database.is_some() {
                return Err(PgError::feature_not_supported(
                    "ALTER ROLE ... IN DATABASE is not supported yet",
                ));
            }
            match config_name {
                ast::ResetConfig::ALL => session.roles.set_config(&name, None, None)?,
                ast::ResetConfig::ConfigName(config) => {
                    let key = config.to_string();
                    let canonical =
                        crate::guc::lookup(&key).map_or(key.clone(), |d| d.name.to_string());
                    session.roles.set_config(&name, Some(&canonical), None)?;
                }
            }
        }
        // MS SQL Server's `ADD`/`DROP MEMBER`, which this parser never produces
        // from PostgreSQL's `ALTER ROLE` grammar.
        ast::AlterRoleOperation::AddMember { .. } | ast::AlterRoleOperation::DropMember { .. } => {
            return Err(PgError::feature_not_supported(
                "ALTER ROLE ... ADD/DROP MEMBER is not supported",
            ));
        }
    }
    Ok(QueryResult::command("ALTER ROLE"))
}

pub(crate) fn execute_drop_role(
    session: &mut Session,
    names: &[ast::ObjectName],
    if_exists: bool,
) -> Result<QueryResult, PgError> {
    let mut notices = Vec::new();
    for name in names {
        let name = single_object_name(name, "role")?;
        notices.extend(session.roles.drop_role(
            &name,
            if_exists,
            session.user_oid(),
            session.current_user_oid(),
        )?);
    }
    Ok(QueryResult::Command {
        tag: "DROP ROLE".into(),
        notices: to_notices(notices),
    })
}

/// Only the role-membership form. A `GRANT` with an `ON <objects>` clause is a
/// privilege grant, and privileges on objects are not modelled — accepting one
/// silently would tell a client its `GRANT SELECT` took effect when nothing
/// checks privileges at all.
pub(crate) fn execute_grant(
    session: &mut Session,
    grant: &ast::Grant,
) -> Result<QueryResult, PgError> {
    let roles = membership_roles(&grant.privileges, grant.objects.as_ref())?;
    let members = grantee_names(&grant.grantees)?;
    if grant.with_grant_option {
        return Err(PgError::syntax(
            "WITH GRANT OPTION is not applicable to role membership",
        ));
    }
    if grant.as_grantor.is_some() {
        return Err(PgError::feature_not_supported(
            "GRANT ... AS <grantor> is not supported",
        ));
    }
    let grantor = grantor(session, grant.granted_by.as_ref(), &roles)?;
    let notices =
        session
            .roles
            .grant_membership(&roles, &members, grant.with_admin_option, grantor)?;
    Ok(QueryResult::Command {
        tag: "GRANT".into(),
        notices: to_notices(notices),
    })
}

/// See [`execute_grant`] for why the object form is refused.
pub(crate) fn execute_revoke(
    session: &mut Session,
    revoke: &ast::Revoke,
) -> Result<QueryResult, PgError> {
    let roles = membership_roles(&revoke.privileges, revoke.objects.as_ref())?;
    let members = grantee_names(&revoke.grantees)?;
    let grantor = grantor(session, revoke.granted_by.as_ref(), &roles)?;
    let warnings =
        session
            .roles
            .revoke_membership(&roles, &members, revoke.admin_option_for, grantor)?;
    Ok(QueryResult::Command {
        tag: "REVOKE".into(),
        notices: warnings
            .into_iter()
            .map(|message| Notice::warning(sqlstate::WARNING, message))
            .collect(),
    })
}

/// Whose grant a `GRANT`/`REVOKE` of role membership is: the role named by
/// `GRANTED BY`, or the one running the statement.
///
/// A trust session is the one identity that cannot be recorded as it stands —
/// its OID is synthetic and no `pg_authid` row keeps it once a real role takes
/// the name — so it grants as the bootstrap superuser, whose row is always
/// there for `pg_auth_members.grantor` to resolve.
fn grantor(
    session: &Session,
    granted_by: Option<&ast::Ident>,
    granted: &[String],
) -> Result<u32, PgError> {
    let current = match session.current_user_oid() {
        crate::roles::TRUST_SESSION_ROLE_OID => crabgresql_catalog::BOOTSTRAP_ROLE_OID,
        oid => oid,
    };
    match granted_by {
        Some(named) => session
            .roles
            .resolve_grantor(&normalize_ident(named), current, granted),
        None => Ok(current),
    }
}

/// The role names a membership `GRANT`/`REVOKE` names, or the error the same
/// statement earns when it is really an object-privilege one.
fn membership_roles(
    privileges: &ast::Privileges,
    objects: Option<&ast::GrantObjects>,
) -> Result<Vec<String>, PgError> {
    if objects.is_some() {
        // An identifier in privilege position with an `ON` clause after it is a
        // misspelled privilege, which is what PostgreSQL calls it.
        if let ast::Privileges::Actions(actions) = privileges
            && let Some(ast::Action::Role { role }) = actions.first()
        {
            return Err(PgError::syntax(format!(
                "unrecognized privilege type \"{role}\""
            )));
        }
        return Err(PgError::feature_not_supported(
            "GRANT/REVOKE of privileges on objects is not supported yet",
        ));
    }
    let ast::Privileges::Actions(actions) = privileges else {
        // `GRANT ALL TO r` — `ALL` needs an object class to expand over.
        return Err(PgError::syntax("syntax error at or near \"ALL\""));
    };
    actions
        .iter()
        .map(|action| match action {
            ast::Action::Role { role } => single_object_name(role, "role"),
            other => Err(PgError::syntax(format!(
                "unrecognized role name \"{other}\""
            ))),
        })
        .collect()
}

/// The names on the `TO`/`FROM` side. `PUBLIC` is a pseudo-role: nothing can be
/// a member of it, and privileges — the only thing granting *to* it means — are
/// not modelled.
fn grantee_names(grantees: &[ast::Grantee]) -> Result<Vec<String>, PgError> {
    grantees
        .iter()
        .map(|grantee| match (&grantee.grantee_type, &grantee.name) {
            (ast::GranteesType::Public, _) => Err(PgError::feature_not_supported(
                "granting to PUBLIC is not supported yet",
            )),
            (_, Some(ast::GranteeName::ObjectName(name))) => single_object_name(name, "role"),
            (_, Some(other)) => Err(PgError::syntax(format!("invalid role name: {other}"))),
            (_, None) => Err(PgError::syntax("missing role name")),
        })
        .collect()
}

/// `SET ROLE` / `SET SESSION AUTHORIZATION`, and the `RESET` spellings of both.
/// Both are ordinary GUC assignments here, as they are in PostgreSQL — which is
/// what makes `SET LOCAL ROLE` roll back with its transaction and `SHOW role`
/// answer.
pub(crate) fn execute_set_role(
    session: &mut Session,
    context_modifier: Option<ast::ContextModifier>,
    role_name: Option<&ast::Ident>,
) -> Result<QueryResult, PgError> {
    let value = match role_name {
        // `SET ROLE DEFAULT` is a syntax error in PostgreSQL — `DEFAULT` is a
        // reserved word its `role_spec` production does not accept, and the
        // `SET role = DEFAULT` spelling is the one that resets. The quoted form
        // still names a role, so only a bare identifier is refused.
        Some(name) if name.quote_style.is_none() && name.value.eq_ignore_ascii_case("default") => {
            return Err(PgError::syntax("syntax error at or near \"DEFAULT\"").at(name.span));
        }
        // `SET ROLE NONE` arrives as an ordinary name; the setter is what reads
        // it as "no role in effect".
        Some(name) => crate::guc::GucValue::Str(normalize_ident(name)),
        None => crate::guc::GucValue::Default,
    };
    assign(session, "role", value, context_modifier)
}

pub(crate) fn execute_set_session_authorization(
    session: &mut Session,
    param: &ast::SetSessionAuthorizationParam,
) -> Result<QueryResult, PgError> {
    let value = match &param.kind {
        ast::SetSessionAuthorizationParamKind::Default => crate::guc::GucValue::Default,
        ast::SetSessionAuthorizationParamKind::User(name) => {
            crate::guc::GucValue::Str(normalize_ident(name))
        }
    };
    assign(session, "session_authorization", value, Some(param.scope))
}

fn assign(
    session: &mut Session,
    key: &str,
    value: crate::guc::GucValue,
    context_modifier: Option<ast::ContextModifier>,
) -> Result<QueryResult, PgError> {
    let def = crate::guc::lookup(key).expect("a parameter this module names is defined");
    let local = context_modifier == Some(ast::ContextModifier::Local);
    if local && session.tx_status == crabgresql_pg_wire::TransactionStatus::Idle {
        // PG warns and changes nothing: there is no transaction for the setting
        // to be local to. Same answer as the general `SET` path gives.
        return Ok(QueryResult::Command {
            tag: "SET".into(),
            notices: vec![Notice::warning(
                sqlstate::NO_ACTIVE_SQL_TRANSACTION,
                "SET LOCAL can only be used in transaction blocks",
            )],
        });
    }
    session.assign_guc(def, value, local)?;
    Ok(QueryResult::command("SET"))
}

/// `CREATE USER` differs from `CREATE ROLE` in exactly one way — it implies
/// `LOGIN` — and the parser records that as a `login` of `Some(true)`, so
/// nothing here has to tell the two spellings apart.
fn role_options(create: &ast::CreateRole, session: &Session) -> Result<RoleOptions, PgError> {
    Ok(RoleOptions {
        superuser: create.superuser,
        inherit: create.inherit,
        createrole: create.create_role,
        createdb: create.create_db,
        canlogin: create.login,
        replication: create.replication,
        bypassrls: create.bypassrls,
        connlimit: create
            .connection_limit
            .as_ref()
            .map(connection_limit)
            .transpose()?,
        password: create.password.as_ref().map(encrypt_password).transpose()?,
        valid_until: create
            .valid_until
            .as_ref()
            .map(|expr| valid_until(expr, session))
            .transpose()?,
    })
}

fn apply_role_option(
    opts: &mut RoleOptions,
    option: &ast::RoleOption,
    session: &Session,
) -> Result<(), PgError> {
    match option {
        ast::RoleOption::BypassRLS(v) => opts.bypassrls = Some(*v),
        ast::RoleOption::CreateDB(v) => opts.createdb = Some(*v),
        ast::RoleOption::CreateRole(v) => opts.createrole = Some(*v),
        ast::RoleOption::Inherit(v) => opts.inherit = Some(*v),
        ast::RoleOption::Login(v) => opts.canlogin = Some(*v),
        ast::RoleOption::Replication(v) => opts.replication = Some(*v),
        ast::RoleOption::SuperUser(v) => opts.superuser = Some(*v),
        ast::RoleOption::ConnectionLimit(expr) => opts.connlimit = Some(connection_limit(expr)?),
        ast::RoleOption::Password(password) => opts.password = Some(encrypt_password(password)?),
        ast::RoleOption::ValidUntil(expr) => opts.valid_until = Some(valid_until(expr, session)?),
    }
    Ok(())
}

/// `PASSWORD 'x'` becomes a SCRAM verifier; `PASSWORD NULL` clears the stored
/// one. Nothing authenticates against either yet — see [`crate::roles::scram`].
fn encrypt_password(password: &ast::Password) -> Result<Option<String>, PgError> {
    match password {
        ast::Password::NullPassword => Ok(None),
        ast::Password::Password(expr) => {
            Ok(Some(scram::encrypt(&string_literal(expr, "password")?)))
        }
    }
}

fn connection_limit(expr: &ast::Expr) -> Result<i32, PgError> {
    signed_literal(expr, "CONNECTION LIMIT")?
        .parse()
        .map_err(|_| PgError::syntax("CONNECTION LIMIT must be an integer"))
}

/// `VALID UNTIL 'timestamp'`, read in the session's time zone as any other
/// `timestamptz` literal is. `'infinity'` is a value like any other here, which
/// is why it is stored rather than turned into NULL: PostgreSQL shows `infinity`
/// in `rolvaliduntil` and has no spelling that puts the column back to NULL.
fn valid_until(expr: &ast::Expr, session: &Session) -> Result<i64, PgError> {
    let literal = string_literal(expr, "VALID UNTIL")?;
    crabgresql_types::timestamptz::parse(&literal, &session.fmt_ctx())
        .map_err(|e| PgError::new(sqlstate::INVALID_DATETIME_FORMAT, e.to_string()))
}

/// The text of a string literal, which is all these clauses accept — as in
/// PostgreSQL's grammar, where they take a constant rather than an expression.
fn string_literal(expr: &ast::Expr, what: &str) -> Result<String, PgError> {
    match expr {
        ast::Expr::Value(value) => match &value.value {
            ast::Value::SingleQuotedString(s) | ast::Value::DoubleQuotedString(s) => Ok(s.clone()),
            ast::Value::Number(n, _) => Ok(n.to_string()),
            other => Err(PgError::syntax(format!(
                "{what} must be a literal: {other}"
            ))),
        },
        other => Err(PgError::syntax(format!(
            "{what} must be a literal: {other}"
        ))),
    }
}

/// The value side of `ALTER ROLE … SET x = <value>`, rendered the way
/// `rolconfig` stores it: unquoted.
fn config_literal(expr: &ast::Expr) -> Result<String, PgError> {
    match expr {
        ast::Expr::Identifier(ident) => Ok(ident.value.clone()),
        other => signed_literal(other, "SET"),
    }
}

/// A numeric or string literal, with the leading `-` the grammar parses as an
/// operator folded back in — `SET extra_float_digits = -3` and
/// `CONNECTION LIMIT -1` both arrive that way.
fn signed_literal(expr: &ast::Expr, what: &str) -> Result<String, PgError> {
    match expr {
        ast::Expr::UnaryOp {
            op: ast::UnaryOperator::Minus,
            expr,
        } => Ok(format!("-{}", string_literal(expr, what)?)),
        other => string_literal(other, what),
    }
}

fn idents(names: &[ast::Ident]) -> Vec<String> {
    names.iter().map(normalize_ident).collect()
}
