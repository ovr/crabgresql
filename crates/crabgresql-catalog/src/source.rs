//! What the server hands the catalog: the shapes of live state, and the trait a
//! session implements to supply it.
//!
//! Kept apart from [`crate::SystemCatalog`] because the direction of dependency
//! is one-way — these types know nothing about how a relation is rendered, and a
//! new wave adds to them without touching the snapshot machinery.

use crabgresql_storage_api::{Column, IndexMetadata, RelStats, RelationMetadata, TableSchema};
use crabgresql_types::PgType;

/// The relation kind reflected into `pg_class.relkind` / `information_schema`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelKind {
    /// An ordinary table (`relkind = 'r'`, table_type `BASE TABLE`).
    Table,
    /// A partitioned (parent) table (`relkind = 'p'`). Holds no rows of its own;
    /// still `BASE TABLE` in `information_schema.tables`, as in PG.
    PartitionedTable,
    /// A view (`relkind = 'v'`, table_type `VIEW`).
    View,
    /// A sequence (`relkind = 'S'`). Not a table, so it is omitted from
    /// `information_schema.tables`/`.columns`.
    Sequence,
}

/// A sequence's parameters, reflected into `pg_sequence`. Carried on the
/// [`CatalogRelation`] whose [`RelKind::Sequence`] entry it belongs to, so the
/// sequence's `pg_class` OID (assigned positionally) can be reused as `seqrelid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogSequence {
    pub type_oid: u32,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
}

/// A live relation exposed through the system catalogs.
#[derive(Clone, Debug)]
pub struct CatalogRelation {
    pub schema: TableSchema,
    pub indexes: Vec<IndexMetadata>,
    pub namespace: String,
    pub temporary: bool,
    pub kind: RelKind,
    /// Sequence parameters, `Some` only when `kind` is [`RelKind::Sequence`].
    pub sequence: Option<CatalogSequence>,
    /// Size estimates feeding `pg_class.relpages`/`reltuples`. Relations with no
    /// storage of their own — views and partitioned parents — leave this at
    /// [`RelStats::unknown`], which renders as PostgreSQL's never-analyzed
    /// sentinel; sequences are reported from their fixed shape instead.
    pub stats: RelStats,
    /// The size of this relation's out-of-line ("TOAST") storage, or `None` if it
    /// has none. `Some` is what gives the relation a `pg_toast.pg_toast_<oid>`
    /// row and a non-zero `pg_class.reltoastrelid`.
    pub toast: Option<RelStats>,
}

/// The relkind of a stored user relation: a partitioned parent (carrying a
/// partition key) is `'p'`, everything else an ordinary table `'r'`. A leaf
/// partition is still an ordinary table (its `partition_of` only sets
/// `relispartition`).
fn table_kind(schema: &TableSchema) -> RelKind {
    if schema.partition_scheme.is_some() {
        RelKind::PartitionedTable
    } else {
        RelKind::Table
    }
}

impl CatalogRelation {
    pub fn permanent(schema: TableSchema) -> Self {
        let namespace = schema.namespace.clone();
        let kind = table_kind(&schema);
        let stats = RelStats::unknown(&schema);
        Self {
            schema,
            indexes: Vec::new(),
            namespace,
            temporary: false,
            kind,
            sequence: None,
            stats,
            toast: None,
        }
    }

    pub fn permanent_metadata(metadata: RelationMetadata) -> Self {
        let namespace = metadata.schema.namespace.clone();
        let kind = table_kind(&metadata.schema);
        Self {
            schema: metadata.schema,
            indexes: metadata.indexes,
            namespace,
            temporary: false,
            kind,
            sequence: None,
            stats: metadata.stats,
            toast: metadata.toast,
        }
    }

    pub fn temporary(schema: TableSchema, namespace: impl Into<String>) -> Self {
        let kind = table_kind(&schema);
        let stats = RelStats::unknown(&schema);
        Self {
            schema,
            indexes: Vec::new(),
            namespace: namespace.into(),
            temporary: true,
            kind,
            sequence: None,
            stats,
            toast: None,
        }
    }

    /// A permanent view. Views have no indexes; its namespace rides on `schema`.
    pub fn view(schema: TableSchema) -> Self {
        let namespace = schema.namespace.clone();
        let stats = RelStats::unknown(&schema);
        Self {
            schema,
            indexes: Vec::new(),
            namespace,
            temporary: false,
            kind: RelKind::View,
            sequence: None,
            stats,
            toast: None,
        }
    }

    /// A permanent sequence in `namespace`. Its `pg_class` shape is PG's three
    /// sequence columns (`last_value`, `log_cnt`, `is_called`); `params` feeds
    /// `pg_sequence`.
    pub fn sequence(
        name: impl Into<String>,
        namespace: impl Into<String>,
        params: CatalogSequence,
    ) -> Self {
        let namespace = namespace.into();
        let schema = TableSchema::in_namespace(
            name,
            namespace.clone(),
            vec![
                Column::new("last_value", PgType::Int8),
                Column::new("log_cnt", PgType::Int8),
                Column::new("is_called", PgType::Bool),
            ],
        );
        let stats = RelStats::unknown(&schema);
        Self {
            schema,
            indexes: Vec::new(),
            namespace,
            temporary: false,
            kind: RelKind::Sequence,
            sequence: Some(params),
            stats,
            toast: None,
        }
    }
}

/// A user-defined type reflected into `pg_type` (and, for enums, `pg_enum`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogUserType {
    pub oid: u32,
    pub name: String,
    /// The enum labels in definition (= sort) order, or `None` for a non-enum
    /// user type (which is not reflected into `pg_type`/`pg_enum` yet).
    pub enum_labels: Option<Vec<String>>,
}

/// A user-defined routine reflected into `pg_proc`. Built by whoever owns the
/// function catalog, so this crate stays free of server types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRoutine {
    pub oid: u32,
    pub name: String,
    pub namespace: String,
    /// `pg_proc.prokind`: `'f'` function, `'p'` procedure.
    pub kind: char,
    /// `pg_proc.prolang` — a `pg_language` OID.
    pub lang: u32,
    /// Input-argument type OIDs, in signature order (`proargtypes`). A `0`
    /// stands for a type that does not resolve, as PostgreSQL reports for one
    /// it cannot name.
    pub arg_types: Vec<u32>,
    /// Every argument including OUT/INOUT, or empty when they are all IN —
    /// PostgreSQL leaves `proallargtypes` NULL in that case.
    pub all_arg_types: Vec<u32>,
    /// One of `i`/`o`/`b` per entry of `all_arg_types`, or empty.
    pub arg_modes: Vec<char>,
    /// Argument names, or empty when no argument is named.
    pub arg_names: Vec<String>,
    pub ret_type: u32,
    pub retset: bool,
    /// `pg_proc.provolatile`: `i`/`s`/`v`.
    pub volatile: char,
    pub strict: bool,
    pub secdef: bool,
    /// `pg_proc.prosrc` — the body as written.
    pub src: String,
}

/// The live server state one [`crate::SystemCatalog`] snapshot reflects.
///
/// Every method is called **at most once** per snapshot, and only when a query
/// actually opens the relation it feeds — `SystemCatalog` owns that
/// memoization, so an implementation is free to be expensive: `relations()`
/// enumerates the whole database, and a `SELECT 1` must never pay for it.
///
/// `relations`, `database` and `owner` have no default on purpose. A silent
/// `Vec::new()` for the first would empty `pg_class`, `pg_attribute` and the
/// information schema at once, and a `"postgres"` default for the other two
/// would label every catalog row with a session identity nobody chose. The
/// optional methods are ones whose empty answer is a truthful "this deployment
/// has none".
pub trait CatalogSource: Send + Sync {
    /// The live user relations to reflect into `pg_class`/`pg_attribute` and
    /// `information_schema`.
    fn relations(&self) -> Vec<CatalogRelation>;

    /// The database this snapshot's session is connected to.
    fn database(&self) -> &str;

    /// The session user. Every catalog row reports it as its owner, and it is
    /// the one name [`crate::SystemCatalog::role_name`] resolves.
    fn owner(&self) -> &str;

    /// The user-defined types to reflect into `pg_type`/`pg_enum`.
    fn user_types(&self) -> Vec<CatalogUserType> {
        Vec::new()
    }

    /// The user-defined routines to reflect into `pg_proc`.
    fn routines(&self) -> Vec<CatalogRoutine> {
        Vec::new()
    }

    /// The user-created schemas (`CREATE SCHEMA`) as `(name, oid)`, to reflect
    /// into `pg_namespace` and `information_schema.schemata`.
    fn schemas(&self) -> Vec<(String, u32)> {
        Vec::new()
    }

    /// The session's open SQL cursors, to reflect into `pg_cursors`.
    fn cursors(&self) -> Vec<CatalogCursor> {
        Vec::new()
    }

    /// The session's prepared statements, to reflect into
    /// `pg_prepared_statements`.
    fn prepared_statements(&self) -> Vec<CatalogPreparedStatement> {
        Vec::new()
    }

    /// The configuration parameters, to reflect into `pg_settings`. The GUC
    /// table lives in the server, so this crate takes the rendered rows rather
    /// than depending on it.
    fn settings(&self) -> Vec<CatalogSetting> {
        Vec::new()
    }

    /// The instant `pg_timezone_names`/`pg_timezone_abbrevs` resolve their
    /// offsets at, in `timestamptz` micros. PostgreSQL reports a zone's offset
    /// and DST flag as of *now*, so a session supplies its transaction
    /// timestamp — `now()`, not `statement_timestamp()` — and the view agrees
    /// with `now()` for every statement in the block.
    fn now(&self) -> i64 {
        crabgresql_types::tz::now_micros()
    }
}

/// A [`CatalogSource`] over data known up front. Clones its vectors on every
/// call, which is harmless under `SystemCatalog`'s memoization and is why this
/// is for tests, fixtures and empty catalogs rather than for a live server.
#[derive(Clone, Debug)]
pub struct StaticSource {
    relations: Vec<CatalogRelation>,
    database: String,
    owner: String,
    user_types: Vec<CatalogUserType>,
    routines: Vec<CatalogRoutine>,
    schemas: Vec<(String, u32)>,
    cursors: Vec<CatalogCursor>,
    prepared_statements: Vec<CatalogPreparedStatement>,
    settings: Vec<CatalogSetting>,
}

impl Default for StaticSource {
    /// Written out rather than derived: `database` and `owner` default to
    /// `postgres`, not to the empty string.
    fn default() -> Self {
        Self {
            relations: Vec::new(),
            database: "postgres".to_string(),
            owner: "postgres".to_string(),
            user_types: Vec::new(),
            routines: Vec::new(),
            schemas: Vec::new(),
            cursors: Vec::new(),
            prepared_statements: Vec::new(),
            settings: Vec::new(),
        }
    }
}

impl StaticSource {
    pub fn new(relations: Vec<CatalogRelation>) -> Self {
        Self {
            relations,
            ..Self::default()
        }
    }

    pub fn database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    pub fn owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = owner.into();
        self
    }

    pub fn user_types(mut self, user_types: Vec<CatalogUserType>) -> Self {
        self.user_types = user_types;
        self
    }

    pub fn routines(mut self, routines: Vec<CatalogRoutine>) -> Self {
        self.routines = routines;
        self
    }

    pub fn schemas(mut self, schemas: Vec<(String, u32)>) -> Self {
        self.schemas = schemas;
        self
    }

    pub fn cursors(mut self, cursors: Vec<CatalogCursor>) -> Self {
        self.cursors = cursors;
        self
    }

    pub fn prepared_statements(mut self, prepared: Vec<CatalogPreparedStatement>) -> Self {
        self.prepared_statements = prepared;
        self
    }

    pub fn settings(mut self, settings: Vec<CatalogSetting>) -> Self {
        self.settings = settings;
        self
    }
}

impl CatalogSource for StaticSource {
    fn relations(&self) -> Vec<CatalogRelation> {
        self.relations.clone()
    }

    fn database(&self) -> &str {
        &self.database
    }

    fn owner(&self) -> &str {
        &self.owner
    }

    fn user_types(&self) -> Vec<CatalogUserType> {
        self.user_types.clone()
    }

    fn routines(&self) -> Vec<CatalogRoutine> {
        self.routines.clone()
    }

    fn schemas(&self) -> Vec<(String, u32)> {
        self.schemas.clone()
    }

    fn cursors(&self) -> Vec<CatalogCursor> {
        self.cursors.clone()
    }

    fn prepared_statements(&self) -> Vec<CatalogPreparedStatement> {
        self.prepared_statements.clone()
    }

    fn settings(&self) -> Vec<CatalogSetting> {
        self.settings.clone()
    }
}

/// One configuration parameter, as `pg_settings` shows it. Session-local like
/// [`CatalogCursor`]: `setting` and `source` differ per connection.
///
/// Column-for-column with PostgreSQL's view, minus `sourcefile`, `sourceline`
/// and `pending_restart` — crabgresql has no configuration file and nothing
/// that needs a restart, so those three are constants the row builder supplies
/// rather than fields every caller would fill identically.
///
/// The metadata borrows rather than owning: it comes straight off the server's
/// `static GUCS` table, so `&'static str` is both free and a guarantee that
/// `pg_settings` shows the same strings `SHOW` does. Only `setting` and
/// `reset_val` are session-derived.
#[derive(Clone, Debug)]
pub struct CatalogSetting {
    /// Canonical spelling (`TimeZone`), not the lower-cased lookup key.
    pub name: &'static str,
    /// The current value, rendered exactly as `SHOW` prints it.
    pub setting: String,
    pub unit: Option<&'static str>,
    pub category: &'static str,
    pub short_desc: &'static str,
    pub extra_desc: Option<&'static str>,
    pub context: &'static str,
    pub vartype: &'static str,
    /// `default` or `session`.
    pub source: &'static str,
    pub min_val: Option<&'static str>,
    pub max_val: Option<&'static str>,
    /// `None` unless `vartype` is `enum`, in PostgreSQL's declaration order.
    pub enumvals: Option<&'static [&'static str]>,
    pub boot_val: &'static str,
    /// What `RESET <name>` would leave behind.
    pub reset_val: String,
}

/// One open cursor, as `pg_cursors` shows it. Cursors are session-local in
/// PostgreSQL too, so this is never shared between connections.
#[derive(Clone, Debug)]
pub struct CatalogCursor {
    pub name: String,
    /// The `DECLARE` statement text.
    pub statement: String,
    pub is_holdable: bool,
    pub is_binary: bool,
    pub is_scrollable: bool,
    /// When the cursor was declared, in `timestamptz` micros.
    pub creation_time: i64,
}

/// One prepared statement, as `pg_prepared_statements` shows it. Session-local
/// in PostgreSQL too, and holding both spellings — the SQL `PREPARE` and the
/// extended protocol's `Parse`, told apart by `from_sql`.
#[derive(Clone, Debug)]
pub struct CatalogPreparedStatement {
    pub name: String,
    /// The statement text, as the session recorded it.
    pub statement: String,
    /// When the statement was prepared, in `timestamptz` micros.
    pub prepare_time: i64,
    /// Type OID per `$n`, rendered as `regtype[]`.
    pub parameter_types: Vec<u32>,
    /// Type OID per result column, or `None` for a statement that returns no
    /// rows — which PostgreSQL reports as NULL, not as an empty array.
    pub result_types: Option<Vec<u32>>,
    pub from_sql: bool,
    pub generic_plans: i64,
    pub custom_plans: i64,
}
