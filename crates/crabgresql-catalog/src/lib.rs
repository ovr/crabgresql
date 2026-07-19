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

use crabgresql_storage_api::{StorageError, TableAm, TableEngine, TableSchema};
use crabgresql_types::Value;

pub use static_table::StaticTable;

/// First OID handed to a user relation in `pg_class`. Matches PostgreSQL's
/// user-object floor; the exact values are synthetic (we have no persistent
/// `pg_class` OIDs yet) but stable within one catalog snapshot.
const FIRST_REL_OID: u32 = 16384;

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

/// A live relation exposed through the system catalogs.
#[derive(Clone, Debug)]
pub struct CatalogRelation {
    pub schema: TableSchema,
    pub namespace: String,
    pub temporary: bool,
}

impl CatalogRelation {
    pub fn permanent(schema: TableSchema) -> Self {
        Self {
            schema,
            namespace: "public".to_string(),
            temporary: false,
        }
    }

    pub fn temporary(schema: TableSchema, namespace: impl Into<String>) -> Self {
        Self {
            schema,
            namespace: namespace.into(),
            temporary: true,
        }
    }
}

/// Produces the live user relations to reflect into `pg_class`/`pg_attribute`
/// and `information_schema`.
/// Boxed so it can capture the server engines and run lazily — only a query that
/// actually opens `pg_class`/`pg_attribute` pays the cost of enumerating them.
type RelationsFn = Box<dyn Fn() -> Vec<CatalogRelation> + Send + Sync>;

/// Read-only engine serving `pg_catalog` relations. Constructed per statement so
/// its rows reflect current server state; live user relations are supplied by a
/// closure that is invoked at most once (and only when `pg_class`/`pg_attribute`
/// is opened), memoized in `oids`.
pub struct SystemCatalog {
    relations: RelationsFn,
    database: String,
    owner: String,
    live_relations: OnceLock<Vec<CatalogRelation>>,
    oids: OnceLock<Vec<(u32, TableSchema)>>,
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
            database: database.into(),
            owner: owner.into(),
            live_relations: OnceLock::new(),
            oids: OnceLock::new(),
        }
    }

    fn live_relations(&self) -> &[CatalogRelation] {
        self.live_relations.get_or_init(|| (self.relations)())
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

    /// Build the requested relation's rows + schema, or `None` if unknown.
    fn build_pg_catalog(&self, name: &str) -> Option<(TableSchema, Vec<Vec<Value>>)> {
        match name {
            "pg_type" => Some((schema::pg_type_schema(), schema::pg_type_builtin_rows())),
            "pg_namespace" => Some((schema::pg_namespace_schema(), schema::pg_namespace_rows())),
            "pg_class" => Some((
                schema::pg_class_schema(),
                schema::pg_class_rows(self.relation_oids()),
            )),
            "pg_attribute" => Some((
                schema::pg_attribute_schema(),
                schema::pg_attribute_rows(self.relation_oids()),
            )),
            "pg_cast" => Some((schema::pg_cast_schema(), schema::pg_cast_rows())),
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

    fn drop_table(&self, name: &str) -> Result<(), StorageError> {
        // The session catalog routes DROP through temp/global, never here; a
        // system catalog relation is not droppable.
        unreachable!("cannot drop relation \"{name}\" from the system catalog")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn pg_class_and_pg_attribute_agree_on_relation_oids() {
        use crabgresql_storage_api::{Column, TableSchema};
        use crabgresql_types::PgType;

        let rels = vec![
            TableSchema {
                name: "beta".to_string(),
                columns: vec![Column::new("x", PgType::Int4)],
            },
            TableSchema {
                name: "alpha".to_string(),
                columns: vec![
                    Column::new("id", PgType::Int4),
                    Column::new("label", PgType::Text),
                ],
            },
        ];
        let cat = SystemCatalog::with_relations(rels);

        let class_schema = schema::pg_class_schema();
        let class = cat.build_pg_catalog("pg_class").unwrap().1;
        let oid_of = |relname: &str| {
            let i = class_schema.column_index("relname").unwrap();
            let o = class_schema.column_index("oid").unwrap();
            class
                .iter()
                .find(|r| r[i] == Value::Text(relname.to_string()))
                .map(|r| r[o].clone())
                .unwrap()
        };
        // Sorted by name → alpha gets the first OID, beta the next.
        assert_eq!(oid_of("alpha"), Value::Oid(FIRST_REL_OID));
        assert_eq!(oid_of("beta"), Value::Oid(FIRST_REL_OID + 1));

        // pg_attribute's attrelid must match pg_class.oid for the same relation.
        let attr_schema = schema::pg_attribute_schema();
        let attr = cat.build_pg_catalog("pg_attribute").unwrap().1;
        let arel = attr_schema.column_index("attrelid").unwrap();
        let aname = attr_schema.column_index("attname").unwrap();
        let anum = attr_schema.column_index("attnum").unwrap();
        let atypid = attr_schema.column_index("atttypid").unwrap();
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
    fn pg_cast_resolves_type_names_to_oids() {
        let schema = schema::pg_cast_schema();
        let rows = schema::pg_cast_rows();
        let src = schema.column_index("castsource").unwrap();
        let tgt = schema.column_index("casttarget").unwrap();
        let ctx = schema.column_index("castcontext").unwrap();
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
    fn information_schema_reflects_relation_metadata() {
        use crabgresql_storage_api::{Column, TableSchema};
        use crabgresql_types::PgType;

        let cat = SystemCatalog::with_catalog_relations_fn("appdb", "appuser", || {
            vec![
                CatalogRelation::permanent(TableSchema {
                    name: "widgets".to_string(),
                    columns: vec![
                        Column::new("id", PgType::Int4),
                        Column::with_typmod("label", PgType::Varchar, 12),
                    ],
                }),
                CatalogRelation::temporary(
                    TableSchema {
                        name: "scratch".to_string(),
                        columns: vec![Column::new("created_at", PgType::TimestampTz)],
                    },
                    "pg_temp_42",
                ),
            ]
        });

        let (tables_schema, tables) = cat.build_information_schema("tables").unwrap();
        assert_eq!(tables_schema.columns.len(), 12);
        let catalog = tables_schema.column_index("table_catalog").unwrap();
        let namespace = tables_schema.column_index("table_schema").unwrap();
        let name = tables_schema.column_index("table_name").unwrap();
        let kind = tables_schema.column_index("table_type").unwrap();
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

        let (columns_schema, columns) = cat.build_information_schema("columns").unwrap();
        assert_eq!(columns_schema.columns.len(), 44);
        assert!(
            columns
                .iter()
                .all(|row| row.len() == columns_schema.columns.len())
        );
        let table_name = columns_schema.column_index("table_name").unwrap();
        let column_name = columns_schema.column_index("column_name").unwrap();
        let ordinal = columns_schema.column_index("ordinal_position").unwrap();
        let data_type = columns_schema.column_index("data_type").unwrap();
        let char_length = columns_schema
            .column_index("character_maximum_length")
            .unwrap();
        let udt_schema = columns_schema.column_index("udt_schema").unwrap();
        let is_generated = columns_schema.column_index("is_generated").unwrap();
        let label = columns
            .iter()
            .find(|row| {
                row[table_name] == Value::Text("widgets".to_string())
                    && row[column_name] == Value::Text("label".to_string())
            })
            .unwrap();
        assert_eq!(label[ordinal], Value::Int4(2));
        assert_eq!(
            label[data_type],
            Value::Text("character varying".to_string())
        );
        assert_eq!(label[char_length], Value::Int4(12));
        assert_eq!(label[udt_schema], Value::Text("pg_catalog".to_string()));
        assert_eq!(label[is_generated], Value::Text("NEVER".to_string()));

        let (_, schemata) = cat.build_information_schema("schemata").unwrap();
        assert!(schemata.iter().any(|row| {
            row[1] == Value::Text("pg_temp_42".to_string())
                && row[2] == Value::Text("appuser".to_string())
        }));
    }
}
