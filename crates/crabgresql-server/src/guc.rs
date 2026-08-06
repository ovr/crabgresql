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
//!
//! A recognized name we do not model is [`GucKind::AcceptedAndIgnored`] for the
//! same reason: `SET client_encoding = 'UTF8'` opens every `pg_dump` file, and
//! rejecting it would break restores that worked before this table existed.

use crabgresql_pg_wire::sqlstate;
use crabgresql_types::interval::{INTERVAL_STYLE_VALUES, IntervalStyle};
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

/// A parameter's previous value, captured verbatim from the session.
///
/// Typed rather than rendered: `SHOW TimeZone` on a zone built by
/// `SET TIME ZONE 7` prints the POSIX spec `<+07>-07`, which is a *display*
/// form that no setter can parse back. Round-tripping through it lost the zone
/// on rollback, silently. Keeping the value makes the restore infallible.
#[derive(Clone)]
pub enum SavedValue {
    TimeZone(std::sync::Arc<SessionZone>),
    ExtraFloatDigits(i32),
    IntervalStyle(IntervalStyle),
    DefaultIsolation(IsolationLevel),
    DefaultReadOnly(bool),
}

/// How a parameter responds to `SET` and `RESET`.
pub enum GucKind {
    /// Backed by session state: `set` applies a new value, and the
    /// `capture`/`restore` pair snapshots and reinstates the old one for the
    /// transactional save stack.
    Settable {
        set: fn(&mut Session, GucValue) -> Result<(), PgError>,
        capture: fn(&Session) -> SavedValue,
        restore: fn(&mut Session, SavedValue),
    },
    /// Accepted and ignored. These are `PGC_USERSET` in PostgreSQL and appear in
    /// every `pg_dump` preamble (`SET client_encoding = 'UTF8';`), but we model
    /// only the one value we implement, so assigning them is a no-op rather than
    /// an error — matching the blanket acceptance this table replaced.
    /// `SHOW` keeps reporting the implemented value.
    AcceptedAndIgnored,
    /// Truly read-only (PG's `PGC_INTERNAL`): `SET` and `RESET <name>` both
    /// raise `55P02`.
    ReadOnly,
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
    pub kind: GucKind,
}

impl GucDef {
    /// Apply a `SET`.
    pub fn set(&self, session: &mut Session, value: GucValue) -> Result<(), PgError> {
        match self.kind {
            GucKind::Settable { set, .. } => set(session, value),
            GucKind::AcceptedAndIgnored => Ok(()),
            GucKind::ReadOnly => Err(cannot_be_changed(self.name)),
        }
    }

    /// Snapshot the current value, for the transactional save stack.
    pub fn capture(&self, session: &Session) -> Option<SavedValue> {
        match self.kind {
            GucKind::Settable { capture, .. } => Some(capture(session)),
            _ => None,
        }
    }

    /// Reinstate a value from [`GucDef::capture`]. Infallible by construction.
    pub fn restore(&self, session: &mut Session, value: SavedValue) {
        if let GucKind::Settable { restore, .. } = self.kind {
            restore(session, value);
        }
    }

    /// Apply a `RESET <name>`, which is `SET … = DEFAULT`. A read-only parameter
    /// raises here, as in PG — unlike `RESET ALL`, which skips it (see
    /// [`GucDef::reset_in_all`]).
    pub fn reset(&self, session: &mut Session) -> Result<(), PgError> {
        self.set(session, GucValue::Default)
    }

    /// `RESET ALL` restores every parameter it can and silently skips the rest,
    /// where `RESET <name>` on the same parameter would error.
    pub fn reset_in_all(&self, session: &mut Session) -> Result<(), PgError> {
        match self.kind {
            GucKind::ReadOnly => Ok(()),
            _ => self.reset(session),
        }
    }

    /// Whether this parameter's value can ever change, and so is worth
    /// snapshotting for the transactional save stack and the ParameterStatus
    /// diff.
    pub fn is_mutable(&self) -> bool {
        matches!(self.kind, GucKind::Settable { .. })
    }
}

pub static GUCS: &[GucDef] = &[
    GucDef {
        key: "timezone",
        name: "TimeZone",
        description: "Sets the time zone for displaying and interpreting time stamps.",
        report: true,
        show: |s| s.timezone.name().to_string(),
        kind: GucKind::Settable {
            set: set_timezone,
            capture: |s| SavedValue::TimeZone(std::sync::Arc::clone(&s.timezone)),
            restore: |s, v| {
                if let SavedValue::TimeZone(z) = v {
                    s.timezone = z;
                }
            },
        },
    },
    GucDef {
        key: "extra_float_digits",
        name: "extra_float_digits",
        description: "Sets the number of digits displayed for floating-point values.",
        report: false,
        show: |s| s.extra_float_digits.to_string(),
        kind: GucKind::Settable {
            set: set_extra_float_digits,
            capture: |s| SavedValue::ExtraFloatDigits(s.extra_float_digits),
            restore: |s, v| {
                if let SavedValue::ExtraFloatDigits(n) = v {
                    s.extra_float_digits = n;
                }
            },
        },
    },
    GucDef {
        key: "intervalstyle",
        name: "IntervalStyle",
        description: "Sets the display format for interval values.",
        // GUC_REPORT in PG: it rides in the startup ParameterStatus burst and
        // every later `SET` echoes it (verified on the wire against PG 18.4).
        report: true,
        show: |s| s.interval_style.name().to_string(),
        kind: GucKind::Settable {
            set: set_interval_style,
            capture: |s| SavedValue::IntervalStyle(s.interval_style),
            restore: |s, v| {
                if let SavedValue::IntervalStyle(x) = v {
                    s.interval_style = x;
                }
            },
        },
    },
    GucDef {
        key: "default_transaction_isolation",
        name: "default_transaction_isolation",
        description: "Sets the transaction isolation level of each new transaction.",
        report: false,
        show: |s| isolation_name(s.default_iso).to_string(),
        kind: GucKind::Settable {
            set: set_default_isolation,
            capture: |s| SavedValue::DefaultIsolation(s.default_iso),
            restore: |s, v| {
                if let SavedValue::DefaultIsolation(l) = v {
                    s.default_iso = l;
                }
            },
        },
    },
    GucDef {
        key: "default_transaction_read_only",
        name: "default_transaction_read_only",
        description: "Sets the default read-only status of new transactions.",
        report: false,
        show: |s| on_off(s.default_read_only),
        kind: GucKind::Settable {
            set: set_default_read_only,
            capture: |s| SavedValue::DefaultReadOnly(s.default_read_only),
            restore: |s, v| {
                if let SavedValue::DefaultReadOnly(b) = v {
                    s.default_read_only = b;
                }
            },
        },
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
        show: |_| crabgresql_types::version::SERVER_VERSION.to_string(),
        kind: GucKind::ReadOnly,
    },
    // psql and most drivers branch on this rather than parsing `server_version`,
    // so the two must agree; `version` owns the pair.
    GucDef {
        key: "server_version_num",
        name: "server_version_num",
        description: "Shows the server version as an integer.",
        report: false,
        show: |_| crabgresql_types::version::SERVER_VERSION_NUM.to_string(),
        kind: GucKind::ReadOnly,
    },
    GucDef {
        key: "server_encoding",
        name: "server_encoding",
        description: "Sets the server (database) character set encoding.",
        report: true,
        show: |_| "UTF8".to_string(),
        kind: GucKind::ReadOnly,
    },
    GucDef {
        key: "client_encoding",
        name: "client_encoding",
        description: "Sets the client's character set encoding.",
        report: true,
        show: |_| "UTF8".to_string(),
        kind: GucKind::AcceptedAndIgnored,
    },
    GucDef {
        key: "datestyle",
        name: "DateStyle",
        description: "Sets the display format for date and time values.",
        report: true,
        show: |_| "ISO, MDY".to_string(),
        kind: GucKind::AcceptedAndIgnored,
    },
    GucDef {
        key: "integer_datetimes",
        name: "integer_datetimes",
        description: "Shows whether datetimes are integer based.",
        report: true,
        show: |_| "on".to_string(),
        kind: GucKind::ReadOnly,
    },
    GucDef {
        key: "standard_conforming_strings",
        name: "standard_conforming_strings",
        description: "Causes '...' strings to treat backslashes literally.",
        report: true,
        show: |_| "on".to_string(),
        kind: GucKind::AcceptedAndIgnored,
    },
    GucDef {
        key: "is_superuser",
        name: "is_superuser",
        description: "Shows whether the current user is a superuser.",
        report: true,
        show: |_| "on".to_string(),
        kind: GucKind::ReadOnly,
    },
];

/// Look a parameter up by name, case-insensitively.
pub fn lookup(name: &str) -> Option<&'static GucDef> {
    let key = name.to_ascii_lowercase();
    GUCS.iter().find(|g| g.key == key)
}

/// Every parameter's current value, keyed by lowercase name — the snapshot
/// `current_setting()` reads during a statement.
///
/// A snapshot rather than a live borrow because the executor holds its handle
/// for the whole statement, and nothing can change a GUC mid-statement: `SET` is
/// itself a statement. Rendering goes through the same `show` functions `SHOW`
/// uses, so the two cannot disagree.
pub fn snapshot(session: &Session) -> std::collections::HashMap<String, String> {
    GUCS.iter()
        .map(|g| (g.key.to_string(), (g.show)(session)))
        .collect()
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
            // `SET extra_float_digits = 0x2` is as valid as `= 2` in PG: the
            // value arrives as the literal's written text, so decode it with
            // the acceptor rather than assuming decimal.
            let v: i32 = crabgresql_binder::literal_int(&s)
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(requires_integer)?;
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

/// `SET IntervalStyle`. The rejection carries PG's HINT listing the four
/// accepted names — without it the message says what is wrong but not what
/// would be right.
fn set_interval_style(session: &mut Session, value: GucValue) -> Result<(), PgError> {
    let rejected = |written: String| {
        invalid_value("IntervalStyle", &written)
            .with_hint(format!("Available values: {INTERVAL_STYLE_VALUES}."))
    };
    session.interval_style = match value {
        GucValue::Default => IntervalStyle::default(),
        GucValue::OffsetSecondsEast(secs) => return Err(rejected(secs.to_string())),
        GucValue::Str(s) => IntervalStyle::from_name(&s).ok_or_else(|| rejected(s.clone()))?,
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
        GucValue::Str(s) => crabgresql_types::parse_bool(&s)
            .ok_or_else(|| requires_boolean("default_transaction_read_only"))?,
    };
    Ok(())
}

fn requires_boolean(param: &str) -> PgError {
    PgError::new(
        sqlstate::INVALID_PARAMETER_VALUE,
        format!("parameter \"{param}\" requires a Boolean value"),
    )
}
