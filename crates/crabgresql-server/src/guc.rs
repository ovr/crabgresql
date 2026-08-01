//! The session configuration parameters `SET`, `RESET` and `SHOW` operate on.
//!
//! Clean-room (see AGENTS.md): reproduces PostgreSQL's *observable* behavior —
//! which names are recognized, what `SHOW` prints, which changes are echoed as
//! `ParameterStatus`, and the SQLSTATE of each rejection — pinned by
//! differential tests against real PG.
//!
//! One table drives all four consumers: the startup `ParameterStatus` burst,
//! `SET`, `RESET`, and `SHOW`. Keeping them in a single list is what stops the
//! startup report and `SHOW` from drifting apart, which is exactly what happened
//! while the startup list was a hardcoded `const` disconnected from `Session`.
//!
//! Two deliberate divergences from PG, both to avoid regressing working
//! clients:
//!
//! * An **unrecognized name is silently accepted** by `SET`/`RESET` (PG raises
//!   `42704`). Drivers set parameters we do not model — `application_name`,
//!   `search_path`, `client_min_messages` — and erroring would break them.
//!   `SHOW` does raise `42704`, since there is no value to invent.
//! * `SHOW ALL` lists only the parameters below, not PG's several hundred.

use crabgresql_pg_wire::sqlstate;
use crabgresql_types::tz::SessionZone;

use crate::error::PgError;
use crabgresql_txn::IsolationLevel;

use crate::session::Session;

/// The value a `SET` supplies, already reduced from the parse tree.
pub enum GucValue {
    /// `SET x = DEFAULT` (and `SET TIME ZONE DEFAULT`/`LOCAL`): the boot value.
    Default,
    /// A string, number, or bare identifier, rendered as written.
    Str(String),
    /// The east-signed offset forms — `SET TIME ZONE 7`, `SET TIME ZONE
    /// INTERVAL '-08:00' HOUR TO MINUTE. Kept distinct from [`GucValue::Str`]
    /// because the sign convention is the opposite one (see [`set_timezone`]).
    OffsetSecondsEast(i32),
}

/// One configuration parameter.
pub struct GucDef {
    /// Lower-cased lookup key. GUC names are case-insensitive in PG.
    pub key: &'static str,
    /// Canonical spelling: the `ParameterStatus` name and `SHOW`'s column name.
    pub name: &'static str,
    /// One-line description, for `SHOW ALL`.
    pub description: &'static str,
    /// PG's `GUC_REPORT`: a change is echoed to the client as `ParameterStatus`.
    pub report: bool,
    pub show: fn(&Session) -> String,
    /// `None` for a read-only parameter, which `SET` rejects with `55P02`.
    pub set: Option<fn(&mut Session, GucValue) -> Result<(), PgError>>,
}

impl GucDef {
    /// `RESET` is `SET … = DEFAULT`; a read-only parameter has nothing to do.
    pub fn reset(&self, session: &mut Session) -> Result<(), PgError> {
        match self.set {
            Some(set) => set(session, GucValue::Default),
            None => Ok(()),
        }
    }
}

pub static GUCS: &[GucDef] = &[
    GucDef {
        key: "timezone",
        name: "TimeZone",
        description: "Sets the time zone for displaying and interpreting time stamps.",
        report: true,
        show: |s| s.timezone.name().to_string(),
        set: Some(set_timezone),
    },
    GucDef {
        key: "extra_float_digits",
        name: "extra_float_digits",
        description: "Sets the number of digits displayed for floating-point values.",
        report: false,
        show: |s| s.extra_float_digits.to_string(),
        set: Some(set_extra_float_digits),
    },
    GucDef {
        key: "default_transaction_isolation",
        name: "default_transaction_isolation",
        description: "Sets the transaction isolation level of each new transaction.",
        report: false,
        show: |s| isolation_name(s.default_iso).to_string(),
        set: Some(set_default_isolation),
    },
    GucDef {
        key: "default_transaction_read_only",
        name: "default_transaction_read_only",
        description: "Sets the default read-only status of new transactions.",
        report: false,
        show: |s| on_off(s.default_read_only),
        set: Some(set_default_read_only),
    },
    // --- reported constants -------------------------------------------------
    // Read-only here, and all `GUC_REPORT` in PG: drivers parse `server_version`
    // and rely on `client_encoding` / `standard_conforming_strings` to pick
    // quoting rules.
    GucDef {
        key: "server_version",
        name: "server_version",
        description: "Shows the server version.",
        report: true,
        show: |_| "19.0 (CrabgreSQL 0.1.0)".to_string(),
        set: None,
    },
    GucDef {
        key: "server_encoding",
        name: "server_encoding",
        description: "Sets the server (database) character set encoding.",
        report: true,
        show: |_| "UTF8".to_string(),
        set: None,
    },
    GucDef {
        key: "client_encoding",
        name: "client_encoding",
        description: "Sets the client's character set encoding.",
        report: true,
        show: |_| "UTF8".to_string(),
        set: None,
    },
    GucDef {
        key: "datestyle",
        name: "DateStyle",
        description: "Sets the display format for date and time values.",
        report: true,
        show: |_| "ISO, MDY".to_string(),
        set: None,
    },
    GucDef {
        key: "integer_datetimes",
        name: "integer_datetimes",
        description: "Shows whether datetimes are integer based.",
        report: true,
        show: |_| "on".to_string(),
        set: None,
    },
    GucDef {
        key: "standard_conforming_strings",
        name: "standard_conforming_strings",
        description: "Causes '...' strings to treat backslashes literally.",
        report: true,
        show: |_| "on".to_string(),
        set: None,
    },
    GucDef {
        key: "is_superuser",
        name: "is_superuser",
        description: "Shows whether the current user is a superuser.",
        report: true,
        show: |_| "on".to_string(),
        set: None,
    },
];

/// Look a parameter up by name, case-insensitively.
pub fn lookup(name: &str) -> Option<&'static GucDef> {
    let key = name.to_ascii_lowercase();
    GUCS.iter().find(|g| g.key == key)
}

/// Every reported parameter's current value — the startup `ParameterStatus`
/// burst, and the baseline [`changed`] diffs against.
pub fn report_values(session: &Session) -> Vec<(String, String)> {
    GUCS.iter()
        .filter(|g| g.report)
        .map(|g| (g.name.to_string(), (g.show)(session)))
        .collect()
}

/// Which reported parameters differ from `before` — the mid-session
/// `ParameterStatus` messages a statement owes the client. Diffing rather than
/// having each setter announce itself means `RESET ALL` and the transactional
/// restore are covered without either knowing about the protocol.
pub fn changed(before: &[(String, String)], session: &Session) -> Vec<(String, String)> {
    report_values(session)
        .into_iter()
        .filter(|(name, value)| {
            !before
                .iter()
                .any(|(prev, prev_value)| prev == name && prev_value == value)
        })
        .collect()
}

/// PG's `55P02` for a parameter that exists but cannot be assigned.
pub fn cannot_be_changed(name: &str) -> PgError {
    PgError::new(
        sqlstate::CANT_CHANGE_RUNTIME_PARAM,
        format!("parameter \"{name}\" cannot be changed"),
    )
}

/// PG's `42704` for a name that is not a configuration parameter at all.
pub fn unrecognized(name: &str) -> PgError {
    PgError::new(
        sqlstate::UNDEFINED_OBJECT,
        format!("unrecognized configuration parameter \"{name}\""),
    )
}

/// PG's `22023` for a recognized parameter given an unusable value.
fn invalid_value(name: &str, value: &str) -> PgError {
    PgError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("invalid value for parameter \"{name}\": \"{value}\""),
    )
}

/// The spelling `SHOW default_transaction_isolation` prints.
fn isolation_name(level: IsolationLevel) -> &'static str {
    match level {
        IsolationLevel::ReadCommitted => "read committed",
        IsolationLevel::RepeatableRead => "repeatable read",
        IsolationLevel::Serializable => "serializable",
    }
}

fn on_off(v: bool) -> String {
    if v { "on" } else { "off" }.to_string()
}

/// `SET TimeZone`.
///
/// The sign trap: a bare numeric *string* is POSIX, counting west
/// (`'+05:30'` means UTC−5:30), while the `SET TIME ZONE 7` and
/// `SET TIME ZONE INTERVAL '…'` forms count east. Both are pinned by
/// differential tests; [`SessionZone::resolve`] owns the first rule and
/// [`SessionZone::from_offset_east`] the second.
fn set_timezone(session: &mut Session, value: GucValue) -> Result<(), PgError> {
    let zone = match value {
        GucValue::Default => SessionZone::utc(),
        GucValue::OffsetSecondsEast(secs) => SessionZone::from_offset_east(secs)
            .map_err(|_| invalid_value("TimeZone", &secs.to_string()))?,
        GucValue::Str(spec) => {
            SessionZone::resolve(&spec).map_err(|_| invalid_value("TimeZone", &spec))?
        }
    };
    session.timezone = std::sync::Arc::new(zone);
    Ok(())
}

fn set_extra_float_digits(session: &mut Session, value: GucValue) -> Result<(), PgError> {
    let requires_integer = || {
        PgError::new(
            sqlstate::INVALID_PARAMETER_VALUE,
            "parameter \"extra_float_digits\" requires an integer value",
        )
    };
    session.extra_float_digits = match value {
        GucValue::Default => 1,
        GucValue::OffsetSecondsEast(_) => return Err(requires_integer()),
        GucValue::Str(s) => {
            let v: i32 = s.trim().parse().map_err(|_| requires_integer())?;
            if !(-15..=3).contains(&v) {
                return Err(PgError::new(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    format!(
                        "{v} is outside the valid range for parameter \"extra_float_digits\" (-15 .. 3)"
                    ),
                ));
            }
            v
        }
    };
    Ok(())
}

fn set_default_isolation(session: &mut Session, value: GucValue) -> Result<(), PgError> {
    session.default_iso = match value {
        GucValue::Default => IsolationLevel::ReadCommitted,
        GucValue::OffsetSecondsEast(_) => {
            return Err(invalid_value("default_transaction_isolation", "<number>"));
        }
        GucValue::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            // `read uncommitted` is an alias: PG never permits dirty reads.
            "read committed" | "read uncommitted" => IsolationLevel::ReadCommitted,
            "repeatable read" => IsolationLevel::RepeatableRead,
            "serializable" => IsolationLevel::Serializable,
            _ => return Err(invalid_value("default_transaction_isolation", &s)),
        },
    };
    Ok(())
}

fn set_default_read_only(session: &mut Session, value: GucValue) -> Result<(), PgError> {
    session.default_read_only = match value {
        GucValue::Default => false,
        GucValue::OffsetSecondsEast(_) => {
            return Err(requires_boolean("default_transaction_read_only"));
        }
        GucValue::Str(s) => {
            parse_bool(&s).ok_or_else(|| requires_boolean("default_transaction_read_only"))?
        }
    };
    Ok(())
}

fn requires_boolean(param: &str) -> PgError {
    PgError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("parameter \"{param}\" requires a Boolean value"),
    )
}

/// PG's boolean GUC spellings.
fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "t" | "yes" | "y" | "1" => Some(true),
        "off" | "false" | "f" | "no" | "n" | "0" => Some(false),
        _ => None,
    }
}
