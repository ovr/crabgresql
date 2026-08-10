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
use crabgresql_types::bytea::ByteaOutput;
use crabgresql_types::interval::IntervalStyle;
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
    ByteaOutput(ByteaOutput),
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
///
/// The metadata below is transcribed from `pg_settings` on a stock PostgreSQL
/// 18.4 rather than invented: `category`, `context`, `vartype`, `extra_desc`,
/// `min_val`/`max_val` and `enumvals` are PostgreSQL facts about a parameter of
/// this name, and a client that reads them expects PostgreSQL's answers. Only
/// `boot_val` may diverge, and only where crabgresql genuinely boots elsewhere
/// — see `TimeZone`.
pub struct GucDef {
    /// Lower-cased lookup key. GUC names are case-insensitive in PG.
    pub key: &'static str,
    /// Canonical spelling: the `ParameterStatus` name and `SHOW`'s column name.
    pub name: &'static str,
    /// One-line description: `SHOW ALL`'s third column, `pg_settings.short_desc`.
    pub description: &'static str,
    /// `pg_settings.extra_desc` — PG's `long_desc`.
    pub extra_desc: Option<&'static str>,
    /// PG's `GUC_REPORT`: a change is echoed to the client as `ParameterStatus`.
    pub report: bool,
    /// The inverse of PG's `GUC_NO_SHOW_ALL`. A parameter flagged there is
    /// readable by name (`SHOW is_superuser` works) but appears in neither
    /// `SHOW ALL` nor `pg_settings`.
    pub show_all: bool,
    /// PG's `config_group`, verbatim.
    pub category: &'static str,
    /// PG's `GucContext`, verbatim: `user` or `internal` for everything here.
    /// Stored rather than derived from [`GucKind`] — the two coincide today
    /// only because no parameter is `postmaster`/`sighup` yet, and the first one
    /// would mislabel itself silently.
    pub context: &'static str,
    /// `bool` | `string` | `integer` | `real` | `enum`.
    pub vartype: &'static str,
    /// Non-NULL only for `integer`/`real`.
    pub min_val: Option<&'static str>,
    pub max_val: Option<&'static str>,
    /// Non-NULL only for `enum`, in PostgreSQL's declaration order.
    pub enumvals: Option<&'static [&'static str]>,
    /// What `RESET` restores, rendered. For a [`GucKind::Settable`] parameter
    /// this must equal what `show` returns right after `set(_, Default)`.
    pub boot_val: &'static str,
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

    /// Whether this parameter's value can ever change, and so is worth
    /// snapshotting for the transactional save stack and the ParameterStatus
    /// diff.
    pub fn is_mutable(&self) -> bool {
        matches!(self.kind, GucKind::Settable { .. })
    }

    /// Whether assigning this parameter raises `55P02`. `RESET <name>` on one
    /// does raise, as in PG — but `RESET ALL` skips it, which is why the caller
    /// needs to ask rather than letting the setter answer.
    pub fn is_read_only(&self) -> bool {
        matches!(self.kind, GucKind::ReadOnly)
    }
}

/// PostgreSQL's `config_group` names, spelled once each.
const LOCALE: &str = "Client Connection Defaults / Locale and Formatting";
const STATEMENT: &str = "Client Connection Defaults / Statement Behavior";
const PRESET: &str = "Preset Options";
const COMPAT: &str = "Version and Platform Compatibility / Previous PostgreSQL Versions";

/// Every parameter this server models, **sorted by name case-insensitively** —
/// the order PostgreSQL's `pg_show_all_settings` returns, and therefore the
/// order both `SHOW ALL` and `pg_settings` inherit for free. `gucs_are_sorted`
/// below fails if an entry is appended out of place.
///
/// The name order is the only grouping: the read-only reported constants
/// (`server_version`, `server_encoding`, `integer_datetimes`, …) are scattered
/// among the settable ones rather than sectioned off. They are all `GUC_REPORT`
/// in PG, which is what drivers rely on to parse the version and pick their
/// quoting rules.
pub static GUCS: &[GucDef] = &[
    GucDef {
        key: "bytea_output",
        name: "bytea_output",
        description: "Sets the output format for bytea.",
        extra_desc: None,
        // Not GUC_REPORT in PG, unlike its neighbours here: a change is not
        // echoed as ParameterStatus (verified on the wire against 18.4, and
        // guarded by `bytea_output_changes_emit_no_parameter_status`).
        report: false,
        show_all: true,
        category: STATEMENT,
        context: "user",
        vartype: "enum",
        min_val: None,
        max_val: None,
        // PostgreSQL's declaration order, which is what `enumvals` prints — and
        // which is alphabetical here only by coincidence, `hex` being the
        // default.
        enumvals: Some(&["escape", "hex"]),
        boot_val: "hex",
        show: |s| s.bytea_output.name().to_string(),
        kind: GucKind::Settable {
            set: set_bytea_output,
            capture: |s| SavedValue::ByteaOutput(s.bytea_output),
            restore: |s, v| {
                if let SavedValue::ByteaOutput(x) = v {
                    s.bytea_output = x;
                }
            },
        },
    },
    GucDef {
        key: "client_encoding",
        name: "client_encoding",
        description: "Sets the client's character set encoding.",
        extra_desc: None,
        report: true,
        show_all: true,
        category: LOCALE,
        context: "user",
        vartype: "string",
        min_val: None,
        max_val: None,
        enumvals: None,
        // PostgreSQL's own boot value, which its `initdb` then overrides — so a
        // stock server also reports `boot_val` SQL_ASCII next to a UTF8
        // `setting`. Reproduced rather than smoothed over.
        boot_val: "SQL_ASCII",
        show: |_| "UTF8".to_string(),
        kind: GucKind::AcceptedAndIgnored,
    },
    GucDef {
        key: "datestyle",
        name: "DateStyle",
        description: "Sets the display format for date and time values.",
        extra_desc: Some("Also controls interpretation of ambiguous date inputs."),
        report: true,
        show_all: true,
        category: LOCALE,
        context: "user",
        vartype: "string",
        min_val: None,
        max_val: None,
        enumvals: None,
        boot_val: "ISO, MDY",
        show: |_| "ISO, MDY".to_string(),
        kind: GucKind::AcceptedAndIgnored,
    },
    GucDef {
        key: "default_transaction_isolation",
        name: "default_transaction_isolation",
        description: "Sets the transaction isolation level of each new transaction.",
        extra_desc: None,
        report: false,
        show_all: true,
        category: STATEMENT,
        context: "user",
        vartype: "enum",
        min_val: None,
        max_val: None,
        // PostgreSQL's declaration order, which is what `enumvals` prints.
        // `read uncommitted` is accepted and behaves as `read committed`, as in
        // PostgreSQL.
        enumvals: Some(&[
            "serializable",
            "repeatable read",
            "read committed",
            "read uncommitted",
        ]),
        boot_val: "read committed",
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
        extra_desc: None,
        report: false,
        show_all: true,
        category: STATEMENT,
        context: "user",
        vartype: "bool",
        min_val: None,
        max_val: None,
        enumvals: None,
        boot_val: "off",
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
    GucDef {
        key: "extra_float_digits",
        name: "extra_float_digits",
        description: "Sets the number of digits displayed for floating-point values.",
        extra_desc: Some(
            "This affects real, double precision, and geometric data types. A zero or negative parameter value is added to the standard number of digits (FLT_DIG or DBL_DIG as appropriate). Any value greater than zero selects precise output mode.",
        ),
        report: false,
        show_all: true,
        category: LOCALE,
        context: "user",
        vartype: "integer",
        // The band `set_extra_float_digits` enforces, published so a client can
        // read it instead of discovering it by being rejected.
        min_val: Some("-15"),
        max_val: Some("3"),
        enumvals: None,
        boot_val: "1",
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
        key: "integer_datetimes",
        name: "integer_datetimes",
        description: "Shows whether datetimes are integer based.",
        extra_desc: None,
        report: true,
        show_all: true,
        category: PRESET,
        context: "internal",
        vartype: "bool",
        min_val: None,
        max_val: None,
        enumvals: None,
        boot_val: "on",
        show: |_| "on".to_string(),
        kind: GucKind::ReadOnly,
    },
    GucDef {
        key: "intervalstyle",
        name: "IntervalStyle",
        description: "Sets the display format for interval values.",
        extra_desc: None,
        // GUC_REPORT in PG: it rides in the startup ParameterStatus burst and
        // every later `SET` echoes it (verified on the wire against PG 18.4).
        report: true,
        show_all: true,
        category: LOCALE,
        context: "user",
        vartype: "enum",
        min_val: None,
        max_val: None,
        // PostgreSQL's declaration order, which is what `enumvals` prints — and
        // also, joined with ", ", the rejection HINT `invalid_enum_value`
        // builds, so the published list and the error text are one declaration.
        enumvals: Some(&["postgres", "postgres_verbose", "sql_standard", "iso_8601"]),
        boot_val: "postgres",
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
        key: "is_superuser",
        name: "is_superuser",
        description: "Shows whether the current user is a superuser.",
        extra_desc: None,
        report: true,
        // PostgreSQL flags this `GUC_NO_SHOW_ALL`: `SHOW is_superuser` answers,
        // but it appears in neither `SHOW ALL` nor `pg_settings`. Verified
        // against 18.4, where `select count(*) from pg_settings where name =
        // 'is_superuser'` is 0.
        show_all: false,
        category: PRESET,
        context: "internal",
        vartype: "bool",
        min_val: None,
        max_val: None,
        enumvals: None,
        boot_val: "off",
        show: |_| "on".to_string(),
        kind: GucKind::ReadOnly,
    },
    GucDef {
        key: "server_encoding",
        name: "server_encoding",
        description: "Sets the server (database) character set encoding.",
        extra_desc: None,
        report: true,
        show_all: true,
        category: PRESET,
        context: "internal",
        vartype: "string",
        min_val: None,
        max_val: None,
        enumvals: None,
        boot_val: "SQL_ASCII",
        show: |_| "UTF8".to_string(),
        kind: GucKind::ReadOnly,
    },
    GucDef {
        key: "server_version",
        name: "server_version",
        description: "Shows the server version.",
        extra_desc: None,
        report: true,
        show_all: true,
        category: PRESET,
        context: "internal",
        vartype: "string",
        min_val: None,
        max_val: None,
        enumvals: None,
        boot_val: crabgresql_types::version::SERVER_VERSION,
        show: |_| crabgresql_types::version::SERVER_VERSION.to_string(),
        kind: GucKind::ReadOnly,
    },
    // psql and most drivers branch on this rather than parsing `server_version`,
    // so the two must agree; `version` owns the pair.
    GucDef {
        key: "server_version_num",
        name: "server_version_num",
        description: "Shows the server version as an integer.",
        extra_desc: None,
        report: false,
        show_all: true,
        category: PRESET,
        context: "internal",
        vartype: "integer",
        // PostgreSQL publishes a preset integer's bounds as the value itself.
        min_val: Some(crabgresql_types::version::SERVER_VERSION_NUM),
        max_val: Some(crabgresql_types::version::SERVER_VERSION_NUM),
        enumvals: None,
        boot_val: crabgresql_types::version::SERVER_VERSION_NUM,
        show: |_| crabgresql_types::version::SERVER_VERSION_NUM.to_string(),
        kind: GucKind::ReadOnly,
    },
    GucDef {
        key: "standard_conforming_strings",
        name: "standard_conforming_strings",
        description: "Causes '...' strings to treat backslashes literally.",
        extra_desc: None,
        report: true,
        show_all: true,
        category: COMPAT,
        context: "user",
        vartype: "bool",
        min_val: None,
        max_val: None,
        enumvals: None,
        boot_val: "on",
        show: |_| "on".to_string(),
        kind: GucKind::AcceptedAndIgnored,
    },
    GucDef {
        key: "timezone",
        name: "TimeZone",
        description: "Sets the time zone for displaying and interpreting time stamps.",
        extra_desc: None,
        report: true,
        show_all: true,
        category: LOCALE,
        context: "user",
        vartype: "string",
        min_val: None,
        max_val: None,
        enumvals: None,
        // PostgreSQL boots at `GMT`; this server boots at `UTC`. `boot_val` must
        // agree with what `RESET TimeZone` actually leaves behind, so reporting
        // PostgreSQL's value here would make the column contradict observable
        // behaviour — a worse divergence than the one it hides.
        boot_val: "UTC",
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

/// Every parameter `pg_settings` shows, in `GUCS` order (already sorted by
/// name, as PostgreSQL's view is).
///
/// Lives here rather than in the catalog crate so every rendering of a GUC —
/// this one, `SHOW`, `SHOW ALL` and `ParameterStatus` — reads the same table
/// through the same `show` functions and cannot disagree.
pub fn catalog_settings(session: &Session) -> Vec<crabgresql_catalog::CatalogSetting> {
    GUCS.iter()
        .filter(|def| def.show_all)
        .map(|def| {
            let setting = (def.show)(session);
            crabgresql_catalog::CatalogSetting {
                name: def.name,
                // `RESET` puts a settable parameter back to its boot value; one
                // that cannot change is already at its reset value, whatever
                // `boot_val` records PostgreSQL booting from.
                reset_val: if def.is_mutable() {
                    def.boot_val.to_string()
                } else {
                    setting.clone()
                },
                setting,
                // No parameter modelled here is measured in kB or ms.
                unit: None,
                category: def.category,
                short_desc: def.description,
                extra_desc: def.extra_desc,
                context: def.context,
                vartype: def.vartype,
                source: if session.guc_is_explicitly_set(def.key) {
                    "session"
                } else {
                    "default"
                },
                min_val: def.min_val,
                max_val: def.max_val,
                enumvals: def.enumvals,
                boot_val: def.boot_val,
            }
        })
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

/// PG's `22023` for an enum parameter given a name outside its `enumvals`,
/// carrying the HINT that lists the accepted spellings.
///
/// The HINT is built from the very `enumvals` `pg_settings` publishes rather
/// than from a hand-written sentence per parameter, so the two cannot disagree:
/// PG's own HINT is exactly that list joined with `", "`, verified against 18.4
/// for both parameters that use this.
///
/// A `key` missing from `GUCS` is a wiring bug, not a runtime condition — it
/// degrades to a HINT-less `22023` rather than panicking, and
/// `every_enum_guc_resolves_its_own_key` below fails at the definition site
/// instead.
fn invalid_enum_value(key: &str, written: &str) -> PgError {
    let Some(def) = lookup(key) else {
        return invalid_value(key, written);
    };
    let error = invalid_value(def.name, written);
    match def.enumvals {
        Some(values) => error.with_hint(format!("Available values: {}.", values.join(", "))),
        None => error,
    }
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
    let rejected = |written: String| invalid_enum_value("intervalstyle", &written);
    session.interval_style = match value {
        GucValue::Default => IntervalStyle::default(),
        GucValue::OffsetSecondsEast(secs) => return Err(rejected(secs.to_string())),
        GucValue::Str(s) => IntervalStyle::from_name(&s).ok_or_else(|| rejected(s.clone()))?,
    };
    Ok(())
}

/// `SET bytea_output`. Like [`set_interval_style`], the rejection carries PG's
/// HINT listing the accepted names.
fn set_bytea_output(session: &mut Session, value: GucValue) -> Result<(), PgError> {
    let rejected = |written: String| invalid_enum_value("bytea_output", &written);
    session.bytea_output = match value {
        GucValue::Default => ByteaOutput::default(),
        GucValue::OffsetSecondsEast(secs) => return Err(rejected(secs.to_string())),
        GucValue::Str(s) => ByteaOutput::from_name(&s).ok_or_else(|| rejected(s.clone()))?,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `GUCS` is the row order of both `SHOW ALL` and `pg_settings`, and
    /// PostgreSQL returns those sorted by name case-insensitively. Keeping the
    /// array itself sorted gives both consumers that order for free — and makes
    /// appending an entry in the wrong place a test failure rather than a
    /// silently diverging expected-file.
    #[test]
    fn gucs_are_sorted_by_name() {
        let names: Vec<String> = GUCS.iter().map(|g| g.name.to_ascii_lowercase()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "GUCS must be sorted by name, case-insensitively"
        );
    }

    /// The lookup key is the canonical name lower-cased. `lookup` scans on
    /// `key`, `pg_settings` and `ParameterStatus` print `name`, and
    /// `Session::explicitly_set` is keyed on `key` — a mismatch would make a
    /// `SET` mark a parameter nothing reads back.
    #[test]
    fn every_key_is_its_name_lowercased() {
        for def in GUCS {
            assert_eq!(def.key, def.name.to_ascii_lowercase(), "{}", def.name);
        }
    }

    /// `invalid_enum_value` looks its parameter up by key to build the HINT
    /// from `enumvals`, and degrades to a HINT-less error if the key is not in
    /// `GUCS`. That degradation must never fire in practice, so pin the two
    /// setters that rely on it: a typo'd key would otherwise drop the HINT from
    /// a live error message and nothing else would notice.
    #[test]
    fn every_enum_guc_resolves_its_own_key() {
        for key in ["bytea_output", "intervalstyle"] {
            let def = lookup(key).unwrap_or_else(|| panic!("{key} is in GUCS"));
            assert_eq!(def.vartype, "enum", "{key}");
            assert!(
                def.enumvals.is_some_and(|v| !v.is_empty()),
                "{key} needs enumvals for its HINT"
            );
        }
    }

    /// Metadata that only makes sense for one `vartype` is present exactly
    /// there: bounds on numbers, `enumvals` on enums.
    #[test]
    fn typed_metadata_matches_vartype() {
        for def in GUCS {
            let numeric = matches!(def.vartype, "integer" | "real");
            assert_eq!(def.min_val.is_some(), numeric, "{}", def.name);
            assert_eq!(def.max_val.is_some(), numeric, "{}", def.name);
            assert_eq!(
                def.enumvals.is_some(),
                def.vartype == "enum",
                "{}",
                def.name
            );
        }
    }
}
