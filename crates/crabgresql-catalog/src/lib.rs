//! `pg_catalog` and `information_schema` system catalogs, served as read-only
//! relations.
//!
//! The core seam: a bound `SELECT` lowers to a scan over an `Arc<dyn TableAm>`,
//! and the executor treats every access method alike. So each supported
//! `pg_catalog` relation is materialized as a [`StaticTable`] (rows built from
//! codegen'd built-in data plus, in later slices, live server state) and handed
//! to the same pipeline user tables use — no bespoke executor node.
//!
//! [`SystemCatalog`] implements [`TableEngine`] so the server's session catalog
//! can layer it into name resolution: `pg_catalog.<rel>` routes here directly,
//! and an unqualified name falls back here (pg_catalog is implicitly on the
//! search path).
//!
//! # Fidelity & clean-room
//!
//! Built-in rows are generated at build time from PostgreSQL's vendored catalog
//! `.dat` *data* (`vendor/postgres/catalog/`), never from its C/Perl source; see
//! `build.rs` and `AGENTS.md`. Column coverage is a curated, PG-ordered subset
//! keyed by the names real clients query; several catalog-only types are
//! represented pragmatically (`oid` is real; `"char"`/`regproc` render as
//! `text`). Full column/type parity with upstream `type_sanity`/`\d` is a
//! follow-up.

mod schema;
mod static_table;

use std::sync::{Arc, OnceLock};

use crabgresql_storage_api::{
    Column, IndexMetadata, RelationMetadata, StorageError, TableAm, TableEngine, TableSchema,
};
use crabgresql_types::{PgType, Value};

pub use static_table::StaticTable;

/// First OID handed to a synthetic user relation in `pg_class`. Runtime type,
/// function, and cast OIDs grow upward from PostgreSQL's user-object floor, so
/// relations use a separate high partition until storage owns persistent OIDs.
/// This preserves catalog-wide uniqueness in every reflected snapshot.
const FIRST_REL_OID: u32 = 0x4000_0000;

#[derive(Clone)]
struct CatalogIndex {
    oid: u32,
    table_oid: u32,
    table_schema: TableSchema,
    metadata: IndexMetadata,
}

/// A built-in `pg_type` row, generated from `pg_type.dat`. Field types mirror
/// the runtime column types in [`schema::pg_type_schema`]; string fields are the
/// catalog `name`/`"char"`/`regproc` text.
pub struct PgTypeRow {
    pub oid: u32,
    pub typname: &'static str,
    pub typnamespace: u32,
    pub typowner: u32,
    pub typlen: i16,
    pub typbyval: bool,
    pub typtype: &'static str,
    pub typcategory: &'static str,
    pub typispreferred: bool,
    pub typisdefined: bool,
    pub typdelim: &'static str,
    pub typrelid: u32,
    pub typelem: u32,
    pub typarray: u32,
    pub typinput: &'static str,
    pub typoutput: &'static str,
    pub typreceive: &'static str,
    pub typsend: &'static str,
    pub typalign: &'static str,
    pub typstorage: &'static str,
}

/// A built-in `pg_cast` row, generated from `pg_cast.dat`. `castsource`/
/// `casttarget` are resolved type OIDs; `castfunc` is the upstream text
/// reference (a `regprocedure`), not yet a resolved OID.
pub struct PgCastRow {
    pub oid: u32,
    pub castsource: u32,
    pub casttarget: u32,
    pub castfunc: &'static str,
    pub castcontext: &'static str,
    pub castmethod: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/pg_type_rows.rs"));
include!(concat!(env!("OUT_DIR"), "/pg_cast_rows.rs"));

/// Whether `name` is the catalog name of a PostgreSQL built-in type, including
/// types crabgresql recognizes but does not implement yet (for example
/// `point`). This distinguishes an unsupported built-in from a nonexistent
/// user type without maintaining a second hand-written name list.
pub fn is_builtin_type_name(name: &str) -> bool {
    PG_TYPE_ROWS.iter().any(|row| row.typname == name)
}

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
        Self {
            schema,
            indexes: Vec::new(),
            namespace,
            temporary: false,
            kind,
            sequence: None,
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
        }
    }

    pub fn temporary(schema: TableSchema, namespace: impl Into<String>) -> Self {
        let kind = table_kind(&schema);
        Self {
            schema,
            indexes: Vec::new(),
            namespace: namespace.into(),
            temporary: true,
            kind,
            sequence: None,
        }
    }

    /// A permanent view. Views have no indexes; its namespace rides on `schema`.
    pub fn view(schema: TableSchema) -> Self {
        let namespace = schema.namespace.clone();
        Self {
            schema,
            indexes: Vec::new(),
            namespace,
            temporary: false,
            kind: RelKind::View,
            sequence: None,
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
        Self {
            schema,
            indexes: Vec::new(),
            namespace,
            temporary: false,
            kind: RelKind::Sequence,
            sequence: Some(params),
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

/// Produces the live user relations to reflect into `pg_class`/`pg_attribute`
/// and `information_schema`.
/// Boxed so it can capture the server engines and run lazily — only a query that
/// actually opens `pg_class`/`pg_attribute` pays the cost of enumerating them.
type RelationsFn = Box<dyn Fn() -> Vec<CatalogRelation> + Send + Sync>;

/// Produces the user-defined types to reflect into `pg_type`/`pg_enum`. Boxed and
/// lazy like [`RelationsFn`] — only a query that opens `pg_type`/`pg_enum` pays.
type UserTypesFn = Box<dyn Fn() -> Vec<CatalogUserType> + Send + Sync>;

/// Produces the user-created schemas (`CREATE SCHEMA`) as `(name, oid)`, to
/// reflect into `pg_namespace` and `information_schema.schemata`. Boxed and lazy
/// like [`RelationsFn`].
type SchemasFn = Box<dyn Fn() -> Vec<(String, u32)> + Send + Sync>;

/// Read-only engine serving `pg_catalog` relations. Constructed per statement so
/// its rows reflect current server state; live user relations are supplied by a
/// closure that is invoked at most once (and only when `pg_class`/`pg_attribute`
/// is opened), memoized in `oids`.
pub struct SystemCatalog {
    relations: RelationsFn,
    user_types_fn: UserTypesFn,
    schemas_fn: SchemasFn,
    database: String,
    owner: String,
    live_relations: OnceLock<Vec<CatalogRelation>>,
    oids: OnceLock<Vec<(u32, TableSchema)>>,
    kinds: OnceLock<Vec<RelKind>>,
    index_oids: OnceLock<Vec<CatalogIndex>>,
    user_types: OnceLock<Vec<CatalogUserType>>,
    user_schemas: OnceLock<Vec<(String, u32)>>,
}

impl Default for SystemCatalog {
    fn default() -> Self {
        Self::with_relations_fn(Vec::new)
    }
}

impl SystemCatalog {
    /// A catalog with no live relations (built-in metadata only).
    pub fn new() -> Self {
        Self::default()
    }

    /// A catalog reflecting a fixed set of live user relations into
    /// `pg_class`/`pg_attribute`.
    pub fn with_relations(relations: Vec<TableSchema>) -> Self {
        Self::with_relations_fn(move || relations.clone())
    }

    /// A catalog that enumerates its live user relations lazily via `f` (invoked
    /// at most once, only if a relation-backed catalog is opened).
    pub fn with_relations_fn(f: impl Fn() -> Vec<TableSchema> + Send + Sync + 'static) -> Self {
        Self::with_catalog_relations_fn("postgres", "postgres", move || {
            f().into_iter().map(CatalogRelation::permanent).collect()
        })
    }

    /// A catalog with session identity and live relation metadata for the
    /// information schema. The callback is memoized per catalog snapshot.
    pub fn with_catalog_relations_fn(
        database: impl Into<String>,
        owner: impl Into<String>,
        f: impl Fn() -> Vec<CatalogRelation> + Send + Sync + 'static,
    ) -> Self {
        Self {
            relations: Box::new(f),
            user_types_fn: Box::new(Vec::new),
            schemas_fn: Box::new(Vec::new),
            database: database.into(),
            owner: owner.into(),
            live_relations: OnceLock::new(),
            oids: OnceLock::new(),
            kinds: OnceLock::new(),
            index_oids: OnceLock::new(),
            user_types: OnceLock::new(),
            user_schemas: OnceLock::new(),
        }
    }

    /// Attach a provider of user-defined types to reflect into `pg_type`/`pg_enum`
    /// (invoked at most once, only if one of those relations is opened).
    pub fn with_user_types_fn(
        mut self,
        f: impl Fn() -> Vec<CatalogUserType> + Send + Sync + 'static,
    ) -> Self {
        self.user_types_fn = Box::new(f);
        self
    }

    /// Attach a provider of user-created schemas to reflect into `pg_namespace`
    /// and `information_schema.schemata` (invoked at most once, only if one of
    /// those relations is opened).
    pub fn with_schemas_fn(
        mut self,
        f: impl Fn() -> Vec<(String, u32)> + Send + Sync + 'static,
    ) -> Self {
        self.schemas_fn = Box::new(f);
        self
    }

    fn live_relations(&self) -> &[CatalogRelation] {
        self.live_relations.get_or_init(|| (self.relations)())
    }

    fn user_types(&self) -> &[CatalogUserType] {
        self.user_types.get_or_init(|| (self.user_types_fn)())
    }

    fn user_schemas(&self) -> &[(String, u32)] {
        self.user_schemas.get_or_init(|| (self.schemas_fn)())
    }

    /// Map every namespace name to its OID: the built-in namespaces plus each
    /// user-created schema. Feeds `pg_class.relnamespace` /
    /// `pg_constraint.connamespace`.
    fn namespace_oids(&self) -> std::collections::HashMap<String, u32> {
        let mut map = std::collections::HashMap::new();
        map.insert("pg_catalog".to_string(), 11);
        map.insert("pg_toast".to_string(), 99);
        map.insert("public".to_string(), 2200);
        for (name, oid) in self.user_schemas() {
            map.insert(name.clone(), *oid);
        }
        map
    }

    /// Assign a stable synthetic OID to each user relation, computed once and
    /// memoized. Sorted by name so `pg_class` and `pg_attribute` (built by
    /// separate `open_table` calls) agree on every relation's OID, keeping their
    /// join consistent.
    fn relation_oids(&self) -> &[(u32, TableSchema)] {
        self.oids.get_or_init(|| {
            let mut rels = self.live_relations().to_vec();
            rels.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            rels.into_iter()
                .enumerate()
                .map(|(i, r)| (FIRST_REL_OID + i as u32, r.schema))
                .collect()
        })
    }

    /// The relation kind for each entry of [`SystemCatalog::relation_oids`], in
    /// the same sorted order, so `pg_class` can emit the right `relkind`.
    fn relation_kinds(&self) -> &[RelKind] {
        self.kinds.get_or_init(|| {
            let mut rels = self.live_relations().to_vec();
            rels.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            rels.into_iter().map(|r| r.kind).collect()
        })
    }

    /// The `(pg_class OID, params)` of each sequence, for `pg_sequence`. The OID
    /// matches the one [`SystemCatalog::relation_oids`] assigns (same sort), so
    /// `pg_sequence.seqrelid` joins `pg_class.oid`.
    fn sequence_entries(&self) -> Vec<(u32, CatalogSequence)> {
        let mut rels = self.live_relations().to_vec();
        rels.sort_by(|a, b| {
            a.namespace
                .cmp(&b.namespace)
                .then_with(|| a.schema.name.cmp(&b.schema.name))
        });
        rels.into_iter()
            .enumerate()
            .filter_map(|(i, r)| r.sequence.map(|s| (FIRST_REL_OID + i as u32, s)))
            .collect()
    }

    fn index_oids(&self) -> &[CatalogIndex] {
        self.index_oids.get_or_init(|| {
            let mut relations = self.live_relations().to_vec();
            relations.sort_by(|a, b| {
                a.namespace
                    .cmp(&b.namespace)
                    .then_with(|| a.schema.name.cmp(&b.schema.name))
            });
            let first_index_oid = FIRST_REL_OID + relations.len() as u32;
            let mut pending = Vec::new();
            for (position, relation) in relations.into_iter().enumerate() {
                let table_oid = FIRST_REL_OID + position as u32;
                for index in relation.indexes {
                    pending.push((table_oid, relation.schema.clone(), index));
                }
            }
            pending.sort_by(|a, b| a.2.name.cmp(&b.2.name));
            pending
                .into_iter()
                .enumerate()
                .map(
                    |(position, (table_oid, table_schema, metadata))| CatalogIndex {
                        oid: first_index_oid + position as u32,
                        table_oid,
                        table_schema,
                        metadata,
                    },
                )
                .collect()
        })
    }

    /// Build the requested relation's rows + schema, or `None` if unknown.
    fn build_pg_catalog(&self, name: &str) -> Option<(TableSchema, Vec<Vec<Value>>)> {
        match name {
            "pg_type" => {
                let mut rows = schema::pg_type_builtin_rows();
                rows.extend(schema::pg_type_user_rows(self.user_types()));
                Some((schema::pg_type_schema(), rows))
            }
            "pg_enum" => Some((
                schema::pg_enum_schema(),
                schema::pg_enum_rows(self.user_types()),
            )),
            "pg_namespace" => Some((
                schema::pg_namespace_schema(),
                schema::pg_namespace_rows(self.user_schemas()),
            )),
            "pg_class" => Some((
                schema::pg_class_schema(),
                schema::pg_class_rows(
                    self.relation_oids(),
                    self.relation_kinds(),
                    self.index_oids(),
                    &self.namespace_oids(),
                ),
            )),
            "pg_attribute" => Some((
                schema::pg_attribute_schema(),
                schema::pg_attribute_rows(self.relation_oids(), self.index_oids()),
            )),
            "pg_attrdef" => Some((
                schema::pg_attrdef_schema(),
                schema::pg_attrdef_rows(self.relation_oids()),
            )),
            "pg_constraint" => Some((
                schema::pg_constraint_schema(),
                schema::pg_constraint_rows(
                    self.relation_oids(),
                    self.index_oids(),
                    &self.namespace_oids(),
                ),
            )),
            "pg_index" => Some((
                schema::pg_index_schema(),
                schema::pg_index_rows(self.index_oids()),
            )),
            "pg_cast" => Some((schema::pg_cast_schema(), schema::pg_cast_rows())),
            "pg_inherits" => Some((
                schema::pg_inherits_schema(),
                schema::pg_inherits_rows(self.relation_oids()),
            )),
            "pg_partitioned_table" => Some((
                schema::pg_partitioned_table_schema(),
                schema::pg_partitioned_table_rows(self.relation_oids()),
            )),
            "pg_sequence" => Some((
                schema::pg_sequence_schema(),
                schema::pg_sequence_rows(&self.sequence_entries()),
            )),
            _ => None,
        }
    }

    fn build_information_schema(&self, name: &str) -> Option<(TableSchema, Vec<Vec<Value>>)> {
        match name {
            "schemata" => Some((
                schema::information_schema_schemata_schema(),
                schema::information_schema_schemata_rows(
                    &self.database,
                    &self.owner,
                    self.live_relations(),
                    self.user_schemas(),
                ),
            )),
            "tables" => Some((
                schema::information_schema_tables_schema(),
                schema::information_schema_tables_rows(&self.database, self.live_relations()),
            )),
            "columns" => Some((
                schema::information_schema_columns_schema(),
                schema::information_schema_columns_rows(&self.database, self.live_relations()),
            )),
            _ => None,
        }
    }
}

impl TableEngine for SystemCatalog {
    fn create_table(&self, schema: TableSchema) -> Result<Arc<dyn TableAm>, StorageError> {
        // The session catalog never routes CREATE here (DDL targets user data).
        unreachable!(
            "cannot create relation \"{}\" in the system catalog",
            schema.name
        )
    }

    fn open_table(&self, name: &str) -> Result<Arc<dyn TableAm>, StorageError> {
        match self.build_pg_catalog(name) {
            Some((schema, rows)) => Ok(StaticTable::arc(schema, rows)),
            None => Err(StorageError::TableNotFound(name.to_string())),
        }
    }

    fn resolve(
        &self,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Arc<dyn TableAm>, StorageError> {
        let relation = match namespace {
            None | Some("pg_catalog") => self.build_pg_catalog(name),
            Some("information_schema") => self.build_information_schema(name),
            Some(_) => None,
        };
        match relation {
            Some((schema, rows)) => Ok(StaticTable::arc(schema, rows)),
            None => Err(StorageError::TableNotFound(name.to_string())),
        }
    }

    fn drop_table(&self, _namespace: &str, name: &str) -> Result<(), StorageError> {
        // The session catalog routes DROP through temp/global, never here; a
        // system catalog relation is not droppable.
        unreachable!("cannot drop relation \"{name}\" from the system catalog")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required<T>(value: Option<T>, message: &str) -> anyhow::Result<T> {
        value.ok_or_else(|| anyhow::anyhow!(message.to_string()))
    }
    use crabgresql_types::Value;

    /// One column of a `pg_type` row, located by column name.
    fn type_col(row: &[Value], schema: &TableSchema, col: &str) -> Value {
        let i = schema.column_index(col).expect("column exists");
        row[i].clone()
    }

    #[test]
    fn pg_type_has_builtin_rows_with_pg_oids() {
        let schema = schema::pg_type_schema();
        let rows = schema::pg_type_builtin_rows();
        let by_name = |name: &str| {
            rows.iter()
                .find(|r| type_col(r, &schema, "typname") == Value::Text(name.to_string()))
                .unwrap_or_else(|| panic!("{name} row present"))
                .clone()
        };
        // Driver-critical OIDs must match PG exactly.
        assert_eq!(type_col(&by_name("int4"), &schema, "oid"), Value::Oid(23));
        assert_eq!(type_col(&by_name("text"), &schema, "oid"), Value::Oid(25));
        assert_eq!(type_col(&by_name("bool"), &schema, "oid"), Value::Oid(16));
        // Metadata columns carry through from pg_type.dat.
        assert_eq!(
            type_col(&by_name("int4"), &schema, "typlen"),
            Value::Int2(4)
        );
        assert_eq!(
            type_col(&by_name("bool"), &schema, "typinput"),
            Value::Text("boolin".to_string())
        );
        // Every row is full-width.
        assert!(rows.iter().all(|r| r.len() == schema.columns.len()));
    }

    #[test]
    fn built_in_name_lookup_includes_unimplemented_types() {
        assert!(is_builtin_type_name("int4"));
        assert!(is_builtin_type_name("point"));
        assert!(!is_builtin_type_name("definitely_not_a_pg_type"));
    }

    #[test]
    fn pg_class_and_pg_attribute_agree_on_relation_oids() -> anyhow::Result<()> {
        use crabgresql_storage_api::{Column, TableSchema};
        use crabgresql_types::PgType;

        let rels = vec![
            TableSchema::new("beta", vec![Column::new("x", PgType::Int4)]),
            TableSchema::new(
                "alpha",
                vec![
                    Column::new("id", PgType::Int4),
                    Column::new("label", PgType::Text),
                ],
            ),
        ];
        let cat = SystemCatalog::with_relations(rels);

        let class_schema = schema::pg_class_schema();
        let class = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?.1;
        let oid_of = |relname: &str| -> anyhow::Result<Value> {
            let i = required(
                class_schema.column_index("relname"),
                "relname column is missing",
            )?;
            let o = required(class_schema.column_index("oid"), "oid column is missing")?;
            required(
                class
                    .iter()
                    .find(|r| r[i] == Value::Text(relname.to_string()))
                    .map(|r| r[o].clone()),
                "relation row is missing",
            )
        };
        // Sorted by name → alpha gets the first OID, beta the next.
        assert_eq!(oid_of("alpha")?, Value::Oid(FIRST_REL_OID));
        assert_eq!(oid_of("beta")?, Value::Oid(FIRST_REL_OID + 1));

        // pg_attribute's attrelid must match pg_class.oid for the same relation.
        let attr_schema = schema::pg_attribute_schema();
        let attr = required(
            cat.build_pg_catalog("pg_attribute"),
            "pg_attribute is missing",
        )?
        .1;
        let arel = required(
            attr_schema.column_index("attrelid"),
            "attrelid column is missing",
        )?;
        let aname = required(
            attr_schema.column_index("attname"),
            "attname column is missing",
        )?;
        let anum = required(
            attr_schema.column_index("attnum"),
            "attnum column is missing",
        )?;
        let atypid = required(
            attr_schema.column_index("atttypid"),
            "atttypid column is missing",
        )?;
        // alpha has two columns, in declared order, tied to alpha's OID.
        let alpha_attrs: Vec<_> = attr
            .iter()
            .filter(|r| r[arel] == Value::Oid(FIRST_REL_OID))
            .collect();
        assert_eq!(alpha_attrs.len(), 2);
        assert_eq!(alpha_attrs[0][aname], Value::Text("id".to_string()));
        assert_eq!(alpha_attrs[0][anum], Value::Int2(1));
        assert_eq!(alpha_attrs[0][atypid], Value::Oid(23)); // int4
        assert_eq!(alpha_attrs[1][atypid], Value::Oid(25)); // text

        Ok(())
    }

    #[test]
    fn pg_type_rows_agree_with_pgtype_for_modeled_types() {
        use crabgresql_types::PgType;
        // Types crabgresql models: their .dat-generated pg_type row must agree
        // with the authoritative PgType::oid()/typlen() used everywhere else, or
        // a pg_attribute.atttypid -> pg_type.oid join silently mismatches.
        let modeled = [
            ("bool", PgType::Bool),
            ("int2", PgType::Int2),
            ("int4", PgType::Int4),
            ("int8", PgType::Int8),
            ("float4", PgType::Float4),
            ("float8", PgType::Float8),
            ("numeric", PgType::Numeric),
            ("text", PgType::Text),
            ("varchar", PgType::Varchar),
            ("bpchar", PgType::Bpchar),
            ("name", PgType::Name),
            ("oid", PgType::Oid),
            ("bytea", PgType::Bytea),
            ("date", PgType::Date),
            ("time", PgType::Time),
            ("timetz", PgType::TimeTz),
            ("timestamp", PgType::Timestamp),
            ("timestamptz", PgType::TimestampTz),
            ("interval", PgType::Interval),
            ("uuid", PgType::Uuid),
            ("inet", PgType::Inet),
            ("cidr", PgType::Cidr),
            ("point", PgType::Point),
            ("lseg", PgType::Lseg),
            ("json", PgType::Json),
            ("jsonb", PgType::Jsonb),
            ("jsonpath", PgType::Jsonpath),
        ];
        for (typname, ty) in modeled {
            let row = PG_TYPE_ROWS
                .iter()
                .find(|r| r.typname == typname)
                .unwrap_or_else(|| panic!("pg_type.dat has a row for {typname}"));
            assert_eq!(row.oid, ty.oid(), "{typname} oid drift (.dat vs PgType)");
            assert_eq!(
                row.typlen,
                ty.typlen(),
                "{typname} typlen drift (.dat vs PgType)"
            );
        }
    }

    #[test]
    fn pg_cast_resolves_type_names_to_oids() -> anyhow::Result<()> {
        let schema = schema::pg_cast_schema();
        let rows = schema::pg_cast_rows();
        let src = required(
            schema.column_index("castsource"),
            "castsource column is missing",
        )?;
        let tgt = required(
            schema.column_index("casttarget"),
            "casttarget column is missing",
        )?;
        let ctx = required(
            schema.column_index("castcontext"),
            "castcontext column is missing",
        )?;
        // int4 (23) -> int8 (20) is an implicit cast in PG.
        let int4_to_int8 = rows
            .iter()
            .find(|r| r[src] == Value::Oid(23) && r[tgt] == Value::Oid(20))
            .expect("int4->int8 cast present");
        assert_eq!(int4_to_int8[ctx], Value::Text("i".to_string()));
        // Every emitted cast references exposed types (nonzero, resolved OIDs).
        assert!(
            rows.iter()
                .all(|r| r[src] != Value::Oid(0) && r[tgt] != Value::Oid(0))
        );

        Ok(())
    }

    #[test]
    fn unknown_relation_is_not_found() {
        let cat = SystemCatalog::new();
        assert!(cat.open_table("pg_type").is_ok());
        assert!(cat.open_table("pg_namespace").is_ok());
        assert!(cat.open_table("pg_cast").is_ok());
        assert!(matches!(
            cat.open_table("pg_nonexistent"),
            Err(StorageError::TableNotFound(_))
        ));
    }

    #[test]
    fn information_schema_reflects_relation_metadata() -> anyhow::Result<()> {
        use crabgresql_storage_api::{Column, TableSchema};
        use crabgresql_types::PgType;

        let cat = SystemCatalog::with_catalog_relations_fn("appdb", "appuser", || {
            vec![
                CatalogRelation::permanent(TableSchema::new(
                    "widgets",
                    vec![
                        Column::new("id", PgType::Int4),
                        Column::with_typmod("label", PgType::Varchar, 12),
                    ],
                )),
                CatalogRelation::temporary(
                    TableSchema::in_namespace(
                        "scratch",
                        "pg_temp_42",
                        vec![Column::new("created_at", PgType::TimestampTz)],
                    ),
                    "pg_temp_42",
                ),
            ]
        });

        let (tables_schema, tables) = required(
            cat.build_information_schema("tables"),
            "information_schema.tables is missing",
        )?;
        assert_eq!(tables_schema.columns.len(), 12);
        let catalog = required(
            tables_schema.column_index("table_catalog"),
            "table_catalog column is missing",
        )?;
        let namespace = required(
            tables_schema.column_index("table_schema"),
            "table_schema column is missing",
        )?;
        let name = required(
            tables_schema.column_index("table_name"),
            "table_name column is missing",
        )?;
        let kind = required(
            tables_schema.column_index("table_type"),
            "table_type column is missing",
        )?;
        assert!(tables.iter().any(|row| {
            row[catalog] == Value::Text("appdb".to_string())
                && row[namespace] == Value::Text("public".to_string())
                && row[name] == Value::Text("widgets".to_string())
                && row[kind] == Value::Text("BASE TABLE".to_string())
        }));
        assert!(tables.iter().any(|row| {
            row[namespace] == Value::Text("pg_temp_42".to_string())
                && row[name] == Value::Text("scratch".to_string())
                && row[kind] == Value::Text("LOCAL TEMPORARY".to_string())
        }));

        let (columns_schema, columns) = required(
            cat.build_information_schema("columns"),
            "information_schema.columns is missing",
        )?;
        assert_eq!(columns_schema.columns.len(), 44);
        assert!(
            columns
                .iter()
                .all(|row| row.len() == columns_schema.columns.len())
        );
        let table_name = required(
            columns_schema.column_index("table_name"),
            "table_name column is missing",
        )?;
        let column_name = required(
            columns_schema.column_index("column_name"),
            "column_name column is missing",
        )?;
        let ordinal = required(
            columns_schema.column_index("ordinal_position"),
            "ordinal column is missing",
        )?;
        let data_type = required(
            columns_schema.column_index("data_type"),
            "data_type column is missing",
        )?;
        let char_length = required(
            columns_schema.column_index("character_maximum_length"),
            "character_maximum_length column is missing",
        )?;
        let udt_schema = required(
            columns_schema.column_index("udt_schema"),
            "udt_schema column is missing",
        )?;
        let is_generated = required(
            columns_schema.column_index("is_generated"),
            "is_generated column is missing",
        )?;
        let label = required(
            columns.iter().find(|row| {
                row[table_name] == Value::Text("widgets".to_string())
                    && row[column_name] == Value::Text("label".to_string())
            }),
            "label column row is missing",
        )?;
        assert_eq!(label[ordinal], Value::Int4(2));
        assert_eq!(
            label[data_type],
            Value::Text("character varying".to_string())
        );
        assert_eq!(label[char_length], Value::Int4(12));
        assert_eq!(label[udt_schema], Value::Text("pg_catalog".to_string()));
        assert_eq!(label[is_generated], Value::Text("NEVER".to_string()));

        let (_, schemata) = required(
            cat.build_information_schema("schemata"),
            "information_schema.schemata is missing",
        )?;
        assert!(schemata.iter().any(|row| {
            row[1] == Value::Text("pg_temp_42".to_string())
                && row[2] == Value::Text("appuser".to_string())
        }));

        Ok(())
    }
}
