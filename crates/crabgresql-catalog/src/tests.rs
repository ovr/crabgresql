//! Tests for the crate root: the registry's invariants, and the catalog rows a
//! snapshot builds end to end.

use super::*;

use crabgresql_storage_api::Column;
use crabgresql_types::PgType;

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
    let schema = catalogs::types::pg_type_schema();
    let rows = catalogs::types::pg_type_builtin_rows();
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
    // `typinput` is a `regproc`: codegen resolved the `.dat` name against
    // `pg_proc.dat`, so it carries PostgreSQL's own OID and prints as the
    // function's name.
    assert_eq!(
        type_col(&by_name("bool"), &schema, "typinput"),
        Value::Reg(crabgresql_types::Reg {
            kind: crabgresql_types::RegKind::Proc,
            oid: 1242,
            name: "boolin".to_string(),
        })
    );
    // The two entries whose alignment pg_type.dat spells symbolically must
    // arrive substituted: PG serves a single character here, never the
    // symbol's name.
    for symbolic in ["internal", "pg_ddl_command"] {
        assert_eq!(
            type_col(&by_name(symbolic), &schema, "typalign"),
            Value::Char(b'd'),
            "{symbolic} typalign must be substituted"
        );
    }
    // typcollation comes from the .dat too — including for types this build
    // does not model, whose collation it is the only source of.
    assert_eq!(
        type_col(&by_name("pg_node_tree"), &schema, "typcollation"),
        Value::Oid(100)
    );
    assert_eq!(
        type_col(&by_name("name"), &schema, "typcollation"),
        Value::Oid(950)
    );
    assert_eq!(
        type_col(&by_name("internal"), &schema, "typcollation"),
        Value::Oid(0)
    );
    // Every row is full-width.
    assert!(rows.iter().all(|r| r.len() == schema.columns.len()));
}

#[test]
fn built_in_name_lookup_includes_unimplemented_types() {
    assert!(is_builtin_type_name("int4"));
    assert!(is_builtin_type_name("point"));
    // An array type is a built-in name in its own right, and one this build
    // resolves: `_int4` declares an integer[] column, as in PostgreSQL.
    assert!(is_builtin_type_name("_int4"));
    assert_eq!(
        crabgresql_types::PgType::from_name("_int4"),
        Some(crabgresql_types::PgType::Array(crabgresql_types::oid::INT4))
    );
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

    let class_schema = catalogs::class::pg_class_schema();
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
    let attr_schema = catalogs::attribute::pg_attribute_schema();
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
    // a pg_attribute.atttypid -> pg_type.oid join silently finds nothing.
    let modeled = [
        ("bool", PgType::Bool),
        ("int2", PgType::Int2),
        ("int4", PgType::Int4),
        ("int8", PgType::Int8),
        ("float4", PgType::Float4),
        ("float8", PgType::Float8),
        ("numeric", PgType::Numeric),
        ("money", PgType::Money),
        ("bit", PgType::Bit),
        ("varbit", PgType::Varbit),
        ("macaddr", PgType::Macaddr),
        ("macaddr8", PgType::Macaddr8),
        ("regclass", PgType::Reg(crabgresql_types::RegKind::Class)),
        ("regtype", PgType::Reg(crabgresql_types::RegKind::Type)),
        (
            "regnamespace",
            PgType::Reg(crabgresql_types::RegKind::Namespace),
        ),
        ("text", PgType::Text),
        ("varchar", PgType::Varchar),
        ("bpchar", PgType::Bpchar),
        ("char", PgType::Char),
        ("name", PgType::Name),
        ("oid", PgType::Oid),
        ("tid", PgType::Tid),
        ("xid", PgType::Xid),
        ("xid8", PgType::Xid8),
        ("pg_lsn", PgType::PgLsn),
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
        ("path", PgType::Path),
        ("box", PgType::Box),
        ("polygon", PgType::Polygon),
        ("line", PgType::Line),
        ("circle", PgType::Circle),
        ("json", PgType::Json),
        ("jsonb", PgType::Jsonb),
        ("jsonpath", PgType::Jsonpath),
        ("tsvector", PgType::Tsvector),
        ("tsquery", PgType::Tsquery),
        (
            "oidvector",
            PgType::Vector(crabgresql_types::VectorKind::Oid),
        ),
        (
            "int2vector",
            PgType::Vector(crabgresql_types::VectorKind::Int2),
        ),
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
        // The name a bare or `pg_catalog.`-qualified type name binds through
        // is the same one the catalog reports, in both directions — so a
        // built-in cannot be spelled one way in `pg_type` and another in a
        // cast.
        assert_eq!(
            PgType::from_name(typname),
            Some(ty),
            "{typname} does not resolve back to its PgType"
        );
        assert_eq!(ty.typname(), typname, "{typname} typname drift");
    }
}

/// `pg_type` and the array-OID table in `crabgresql-types` are two
/// independent statements of the same fact — the first generated from
/// `pg_type.dat`, the second hand-written (it cannot depend on this crate's
/// codegen). Pin them against each other in both directions: an element
/// whose array OID they disagree on would send `PgType::Array` values out on
/// the wire under an OID whose catalog row describes a different element.
#[test]
fn array_rows_agree_with_the_array_oid_table() {
    use crabgresql_types::array::{array_oid_for_elem, elem_oid_for_array};

    let row_for = |oid: u32| PG_TYPE_ROWS.iter().find(|r| r.oid == oid);
    for row in PG_TYPE_ROWS {
        // Element -> array: the table's answer must be the row this build
        // actually emits, and that row must exist.
        if let Some(array_oid) = array_oid_for_elem(row.oid) {
            assert_eq!(
                row.typarray, array_oid,
                "{}: typarray {} but the array-OID table says {array_oid}",
                row.typname, row.typarray
            );
            let array = row_for(array_oid).unwrap_or_else(|| {
                panic!(
                    "no pg_type row for {}'s array (oid {array_oid})",
                    row.typname
                )
            });
            assert_eq!(array.typelem, row.oid, "{} typelem drift", array.typname);
            assert_eq!(array.typname, format!("_{}", row.typname));
        }
        // Array -> element, for the arrays the table models at all.
        if let Some(elem_oid) = elem_oid_for_array(row.oid) {
            assert_eq!(
                row.typelem, elem_oid,
                "{}: typelem {} but the array-OID table says {elem_oid}",
                row.typname, row.typelem
            );
        }
    }
}

/// Every array type gets its own row, derived from its element's. Values
/// pinned against PostgreSQL: an array is a varlena with extended storage
/// and the array I/O functions, it inherits `typdelim` from its element but
/// widens `typalign` to `i` unless the element is double-aligned, and it
/// takes its element's collation.
#[test]
fn array_rows_are_derived_from_their_element() {
    let schema = catalogs::types::pg_type_schema();
    let rows = catalogs::types::pg_type_builtin_rows();
    let by_name = |name: &str| {
        rows.iter()
            .find(|r| type_col(r, &schema, "typname") == Value::Text(name.to_string()))
            .unwrap_or_else(|| panic!("{name} row present"))
            .clone()
    };
    let col = |name: &str, column: &str| type_col(&by_name(name), &schema, column);

    // Driver-critical: _int4 is the OID every client's type map keys on.
    assert_eq!(col("_int4", "oid"), Value::Oid(1007));
    assert_eq!(col("int4", "typarray"), Value::Oid(1007));
    assert_eq!(col("_int4", "typelem"), Value::Oid(23));
    assert_eq!(col("_int4", "typlen"), Value::Int2(-1));
    assert_eq!(col("_int4", "typbyval"), Value::Bool(false));
    assert_eq!(col("_int4", "typcategory"), Value::Char(b'A'));
    // A derived array row's `regproc` columns are the array family's own,
    // resolved to PostgreSQL's OIDs like every other reference.
    assert_eq!(
        col("_int4", "typinput"),
        Value::Reg(crabgresql_types::Reg {
            kind: crabgresql_types::RegKind::Proc,
            oid: 750,
            name: "array_in".to_string(),
        })
    );
    assert_eq!(col("_int4", "typstorage"), Value::Char(b'x'));
    // An array of arrays is not a type of its own.
    assert_eq!(col("_int4", "typarray"), Value::Oid(0));

    // typalign: `i` for everything but a double-aligned element — note bool
    // is `c`, yet _bool is still `i`.
    assert_eq!(col("_int4", "typalign"), Value::Char(b'i'));
    assert_eq!(col("_bool", "typalign"), Value::Char(b'i'));
    assert_eq!(col("_float8", "typalign"), Value::Char(b'd'));
    // typdelim, in contrast, is inherited: box separates with `;`.
    assert_eq!(col("box", "typdelim"), Value::Char(b';'));
    assert_eq!(col("_box", "typdelim"), Value::Char(b';'));

    // An array is collatable exactly when its element is, with the same
    // collation — `name` is C-collated, so `_name` is too.
    assert_eq!(col("_text", "typcollation"), Value::Oid(100));
    assert_eq!(col("_name", "typcollation"), Value::Oid(950));
    assert_eq!(col("_int4", "typcollation"), Value::Oid(0));

    // `_record` is spelled out in pg_type.dat (arrays of records keep
    // typcategory P, so they cannot be autogenerated) — it must come
    // through as that entry, not as a derived row.
    assert_eq!(col("_record", "oid"), Value::Oid(2287));
    assert_eq!(col("_record", "typcategory"), Value::Char(b'P'));
    assert_eq!(col("record", "typarray"), Value::Oid(2287));
}

/// `pg_type.typcollation` comes from the vendored data; `pg_attribute.
/// attcollation` is computed at runtime by `catalogs::collation::typcollation_of`. psql's
/// `\d` compares the two literally (`a.attcollation <> t.typcollation`) to
/// decide whether to print a Collation column, so any drift between them
/// shows up as a spurious collation on every column of the drifted type.
#[test]
fn typcollation_agrees_between_pg_type_and_pg_attribute() {
    use crabgresql_types::PgType;
    for row in PG_TYPE_ROWS {
        // Only types this build models can be a column type at all; the
        // rest have no runtime answer to compare against.
        if PgType::from_oid(row.oid).is_none() {
            continue;
        }
        assert_eq!(
            row.typcollation,
            catalogs::collation::typcollation_of(row.oid),
            "{} typcollation drift (pg_type vs pg_attribute)",
            row.typname
        );
    }
}

#[test]
fn pg_type_rows_are_unique_by_oid_and_name() {
    let mut oids: Vec<u32> = PG_TYPE_ROWS.iter().map(|r| r.oid).collect();
    oids.sort_unstable();
    let before = oids.len();
    oids.dedup();
    assert_eq!(before, oids.len(), "duplicate oid in PG_TYPE_ROWS");

    let mut names: Vec<&str> = PG_TYPE_ROWS.iter().map(|r| r.typname).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "duplicate typname in PG_TYPE_ROWS");
}

/// The pseudo-type table in `crabgresql-types` is hand-written (it cannot
/// depend on this crate's codegen), so pin it against the vendored
/// `pg_type.dat`: every `typtype = 'p'` row must be named, with no extras and
/// no drift in either direction. Without this the table silently rots the next
/// time the catalog is re-vendored.
#[test]
fn pseudo_types_agree_with_the_vendored_catalog() {
    let mut vendored: Vec<(u32, &str)> = PG_TYPE_ROWS
        .iter()
        .filter(|row| row.typtype == "p")
        .map(|row| (row.oid, row.typname))
        .collect();
    vendored.sort();
    assert!(!vendored.is_empty(), "no pseudo-types in the vendored rows");

    for (oid, typname) in &vendored {
        assert!(
            crabgresql_types::pseudo_type_name(*oid).is_some(),
            "pseudo-type {typname} (oid {oid}) is not named"
        );
        assert_eq!(
            crabgresql_types::pseudo_type_oid(typname),
            Some(*oid),
            "{typname} does not resolve back to its oid"
        );
        // A pseudo-type must NOT be a PgType: that is what keeps it
        // undeclarable, since a column type is resolved through `from_name`.
        assert_eq!(
            crabgresql_types::PgType::from_name(typname),
            None,
            "{typname} must not be declarable as a column type"
        );
    }

    // No extras: every named oid is a vendored pseudo-type.
    for (oid, _) in (0..=u32::from(u16::MAX))
        .filter_map(|oid| crabgresql_types::pseudo_type_name(oid).map(|name| (oid, name)))
    {
        assert!(
            vendored.iter().any(|(o, _)| *o == oid),
            "oid {oid} is named as a pseudo-type but is not one in pg_type.dat"
        );
    }
}

#[test]
fn pg_cast_resolves_type_names_to_oids() -> anyhow::Result<()> {
    let schema = catalogs::types::pg_cast_schema();
    let rows = catalogs::types::pg_cast_rows(&SystemCatalog::new());
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
    assert_eq!(int4_to_int8[ctx], Value::Char(b'i'));
    // Every emitted cast references exposed types (nonzero, resolved OIDs).
    assert!(
        rows.iter()
            .all(|r| r[src] != Value::Oid(0) && r[tgt] != Value::Oid(0))
    );

    Ok(())
}

/// Every `regproc`/`regprocedure` reference the catalogs publish resolves to
/// a `pg_proc` row this build actually emits.
///
/// This is the invariant that makes the references worth having: upstream's
/// `oidjoins` test exists to catch exactly this kind of dangling pointer,
/// and codegen picks the emitted `pg_proc` subset *from* these references,
/// so the two can only drift by mistake.
#[test]
fn every_regproc_reference_resolves_to_an_emitted_row() -> anyhow::Result<()> {
    let published: Vec<u32> = catalogs::proc::pg_proc_builtin_rows()
        .iter()
        .map(|r| match r[0] {
            Value::Oid(oid) => oid,
            ref other => panic!("pg_proc.oid is not an OID: {other:?}"),
        })
        .collect();
    let resolves = |value: &Value, what: &str| match value {
        // 0 is a legitimate "no function", which `regprocout` prints as `-`.
        Value::Reg(r) if r.oid == 0 => {}
        Value::Reg(r) => assert!(
            published.contains(&r.oid),
            "{what} points at {} (oid {}), which pg_proc does not publish",
            r.name,
            r.oid
        ),
        Value::Oid(0) => {}
        Value::Oid(oid) => assert!(
            published.contains(oid),
            "{what} points at oid {oid}, which pg_proc does not publish"
        ),
        other => panic!("{what} is not a function reference: {other:?}"),
    };

    // All eight `regproc` columns codegen emits, not just the four I/O ones:
    // `typsubscript` and `typanalyze` are what `oidjoins` joins on for the
    // derived array rows.
    let type_regprocs = [
        "typinput",
        "typoutput",
        "typreceive",
        "typsend",
        "typmodin",
        "typmodout",
        "typanalyze",
        "typsubscript",
    ];
    let type_schema = catalogs::types::pg_type_schema();
    for row in catalogs::types::pg_type_builtin_rows() {
        for col in type_regprocs {
            let i = required(type_schema.column_index(col), "column is missing")?;
            resolves(&row[i], col);
        }
    }
    let cast_schema = catalogs::types::pg_cast_schema();
    let castfunc = required(cast_schema.column_index("castfunc"), "castfunc missing")?;
    for row in catalogs::types::pg_cast_rows(&SystemCatalog::new()) {
        resolves(&row[castfunc], "pg_cast.castfunc");
    }
    let am_schema = catalogs::am::pg_am_schema();
    let amhandler = required(am_schema.column_index("amhandler"), "amhandler missing")?;
    for row in catalogs::am::pg_am_rows(&SystemCatalog::new()) {
        // Unlike a `pg_type` regproc or a cast function, an access method
        // always has a handler — including crabgresql's own, which get rows
        // of their own.
        assert_ne!(
            row[amhandler],
            Value::Reg(crabgresql_types::Reg::unresolved(
                crabgresql_types::RegKind::Proc,
                0
            ))
        );
        resolves(&row[amhandler], "pg_am.amhandler");
    }
    // A user enum's row is built here rather than read from a `.dat`, so
    // codegen cannot record its four I/O references the way it records a
    // generated catalog's — they are declared to it by `crabgresql-bki`'s
    // hand-written-catalog list, and this is what fails when that declaration
    // goes missing. Like an access method's handler, an enum always has all
    // four.
    let enum_row = required(
        catalogs::types::pg_type_user_rows(&[CatalogUserType {
            oid: 20000,
            name: "mood".to_string(),
            enum_labels: Some(vec!["ok".to_string()]),
        }])
        .into_iter()
        .next(),
        "a user enum publishes no pg_type row",
    )?;
    for col in ["typinput", "typoutput", "typreceive", "typsend"] {
        let i = required(type_schema.column_index(col), "column is missing")?;
        assert_ne!(
            enum_row[i],
            Value::Reg(crabgresql_types::Reg::unresolved(
                crabgresql_types::RegKind::Proc,
                0
            )),
            "a user enum's {col} is `-`"
        );
        resolves(&enum_row[i], col);
    }
    Ok(())
}

/// `pg_am` reports PostgreSQL's built-in access methods verbatim, and the
/// OIDs `pg_class.relam` emits are exactly the ones it can be joined to.
#[test]
fn pg_am_lists_the_builtin_access_methods() -> anyhow::Result<()> {
    let schema = catalogs::am::pg_am_schema();
    let rows = catalogs::am::pg_am_rows(&SystemCatalog::new());
    let oid = required(schema.column_index("oid"), "oid column is missing")?;
    let amname = required(schema.column_index("amname"), "amname column is missing")?;
    let amtype = required(schema.column_index("amtype"), "amtype column is missing")?;
    assert!(rows.iter().all(|r| r.len() == schema.columns.len()));

    let by_oid = |n: u32| rows.iter().find(|r| r[oid] == Value::Oid(n));
    // PostgreSQL ships one table access method, heap, and six index ones;
    // crabgresql's parquet and buffer methods are the other two `t` rows.
    let heap = required(by_oid(2), "heap row is missing")?;
    assert_eq!(heap[amname], Value::Text("heap".to_string()));
    assert_eq!(heap[amtype], Value::Char(b't'));
    let btree = required(by_oid(403), "btree row is missing")?;
    assert_eq!(btree[amname], Value::Text("btree".to_string()));
    assert_eq!(btree[amtype], Value::Char(b'i'));
    assert_eq!(
        rows.iter()
            .filter(|r| r[amtype] == Value::Char(b'i'))
            .count(),
        6
    );

    // Every `relam` a pg_class row can carry joins to a pg_am row (0 is the
    // no-access-method sentinel views/sequences/partitioned parents use).
    let cat = SystemCatalog::with_relations(vec![TableSchema::new(
        "t",
        vec![Column::new("a", PgType::Int4)],
    )]);
    let (class_schema, class_rows) =
        required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
    let relam = required(
        class_schema.column_index("relam"),
        "relam column is missing",
    )?;
    for row in &class_rows {
        if row[relam] == Value::Oid(0) {
            continue;
        }
        assert!(
            rows.iter().any(|am| am[oid] == row[relam]),
            "pg_class.relam {:?} has no pg_am row",
            row[relam]
        );
    }

    Ok(())
}

/// `pg_opclass` carries PostgreSQL's own OIDs, and the join a client follows
/// out of an index — `indclass` → `pg_opclass` → `pg_opfamily` → `pg_am` —
/// holds at every hop.
///
/// The 13 OIDs pinned below are the ones `pg_opclass.dat` spells out itself, so
/// they are fixed upstream rather than assigned by codegen. The other 166 are
/// derived (see `crabgresql-bki`'s `pg_opclass`); pinning one of those here
/// would pin a number that moves when upstream inserts an entry above it, which
/// is a property of PostgreSQL, not a regression.
#[test]
fn pg_opclass_reports_postgres_oids_and_joins_to_pg_am() -> anyhow::Result<()> {
    let cat = SystemCatalog::new();
    let (schema, rows) = required(cat.build_pg_catalog("pg_opclass"), "pg_opclass is missing")?;
    let col = |name: &str| schema.column_index(name).expect("column exists");
    let (oid, method, opcname, intype, default) = (
        col("oid"),
        col("opcmethod"),
        col("opcname"),
        col("opcintype"),
        col("opcdefault"),
    );
    let default_class = |am: u32, type_oid: u32| {
        rows.iter()
            .find(|r| {
                r[method] == Value::Oid(am)
                    && r[intype] == Value::Oid(type_oid)
                    && r[default] == Value::Bool(true)
            })
            .map(|r| (r[oid].clone(), r[opcname].clone()))
    };
    for (type_oid, expected_oid, expected_name) in [
        (23_u32, 1978_u32, "int4_ops"),
        (21, 1979, "int2_ops"),
        (26, 1981, "oid_ops"),
        (25, 3126, "text_ops"),
        (1082, 3122, "date_ops"),
        (701, 3123, "float8_ops"),
        (20, 3124, "int8_ops"),
        (1700, 3125, "numeric_ops"),
        (1184, 3127, "timestamptz_ops"),
        (1114, 3128, "timestamp_ops"),
    ] {
        assert_eq!(
            default_class(403, type_oid),
            Some((
                Value::Oid(expected_oid),
                Value::Text(expected_name.to_string())
            )),
            "btree default class for type {type_oid}"
        );
    }

    // Every reference out of the two relations lands on a row that exists:
    // `pg_am` is hand-written and the rest is generated, so this is where the
    // two would be caught disagreeing.
    let (fam_schema, fam_rows) = required(cat.build_pg_catalog("pg_opfamily"), "pg_opfamily")?;
    let (fam_oid, fam_method) = (
        fam_schema.column_index("oid").expect("oid"),
        fam_schema.column_index("opfmethod").expect("opfmethod"),
    );
    let (am_schema, am_rows) = required(cat.build_pg_catalog("pg_am"), "pg_am")?;
    let am_oid = am_schema.column_index("oid").expect("oid");
    let (type_schema, type_rows) = required(cat.build_pg_catalog("pg_type"), "pg_type")?;
    let type_oid_col = type_schema.column_index("oid").expect("oid");
    let known = |haystack: &[Vec<Value>], at: usize, needle: &Value| {
        haystack.iter().any(|r| &r[at] == needle)
    };
    let family = col("opcfamily");
    for row in &rows {
        assert!(known(&am_rows, am_oid, &row[method]), "opcmethod dangles");
        assert!(known(&fam_rows, fam_oid, &row[family]), "opcfamily dangles");
        assert!(
            known(&type_rows, type_oid_col, &row[intype]),
            "opcintype dangles"
        );
    }
    for row in &fam_rows {
        assert!(
            known(&am_rows, am_oid, &row[fam_method]),
            "opfmethod dangles"
        );
    }
    Ok(())
}

/// `pg_index.indclass` names the operator class each key is really built
/// under — the column whose every entry used to be `0`.
///
/// `varchar` is the interesting key: it has no default class of its own
/// (upstream's `varchar_ops` is a non-default alias inside the text family), so
/// PostgreSQL resolves it through the binary-coercible cast to `text` and
/// reports `text_ops`. A lookup that only tried an exact `opcintype` match
/// would report `0` here and look plausible doing it.
#[test]
fn pg_index_indclass_names_each_keys_operator_class() -> anyhow::Result<()> {
    use crabgresql_storage_api::{IndexKey, IndexMethod};
    use crabgresql_types::VectorKind;

    let key = |column: usize| IndexKey {
        column,
        descending: false,
        nulls_first: false,
    };
    let index = |name: &str, method: IndexMethod, keys: Vec<IndexKey>| IndexMetadata {
        name: name.to_string(),
        method,
        keys,
        unique: false,
        nulls_distinct: true,
        constraint: None,
    };
    let mut relation = CatalogRelation::permanent(TableSchema::new(
        "t",
        vec![
            Column::new("i", PgType::Int4),
            Column::new("v", PgType::Varchar),
            Column::new("a", PgType::Array(PgType::Int4.oid())),
            Column::new("m", PgType::User(16_500)),
            Column::new("i2v", PgType::Vector(VectorKind::Int2)),
            Column::new("ov", PgType::Vector(VectorKind::Oid)),
        ],
    ));
    relation.indexes = vec![
        index(
            "t_bt",
            IndexMethod::BTree,
            vec![key(0), key(1), key(2), key(3), key(4), key(5)],
        ),
        index("t_hash", IndexMethod::Hash, vec![key(0)]),
    ];
    let cat = SystemCatalog::from_source(Arc::new(StaticSource::new(vec![relation])));
    let (schema, rows) = required(cat.build_pg_catalog("pg_index"), "pg_index is missing")?;
    let indclass = schema.column_index("indclass").expect("indclass column");
    let indexrelid = schema
        .column_index("indexrelid")
        .expect("indexrelid column");

    let classes = |row: &Vec<Value>| match &row[indclass] {
        Value::Vector { elems, .. } => elems.clone(),
        other => panic!("indclass is not an oidvector: {other:?}"),
    };
    // The rows are ordered by index name, so `t_bt` precedes `t_hash`.
    let btree = classes(&rows[0]);
    assert_eq!(btree[0], Value::Oid(1978), "int4 btree key is int4_ops");
    assert_eq!(
        btree[1],
        Value::Oid(3126),
        "varchar btree key is text_ops, reached by binary coercion"
    );
    let hash = classes(&rows[1]);
    assert_ne!(hash[0], btree[0], "the hash class is not the btree one");

    let (opc_schema, opc_rows) = required(cat.build_pg_catalog("pg_opclass"), "pg_opclass")?;
    let (opc_oid, opc_method) = (
        opc_schema.column_index("oid").expect("oid"),
        opc_schema.column_index("opcmethod").expect("opcmethod"),
    );
    let opc_name = opc_schema.column_index("opcname").expect("opcname");
    let name_of = |class: &Value| {
        opc_rows
            .iter()
            .find(|r| &r[opc_oid] == class)
            .map(|r| r[opc_name].clone())
    };
    // The four keys below are asserted by name because none of their classes
    // carries an upstream OID of its own. An array and an enum have no class
    // of their own and index under a polymorphic one; `int2vector` joins them
    // because PostgreSQL files it under `typcategory` A, while `oidvector` —
    // also category A — has `oidvector_ops` and never gets that far.
    assert_eq!(
        name_of(&btree[2]),
        Some(Value::Text("array_ops".to_string()))
    );
    assert_eq!(
        name_of(&btree[3]),
        Some(Value::Text("enum_ops".to_string()))
    );
    assert_eq!(
        name_of(&btree[4]),
        Some(Value::Text("array_ops".to_string()))
    );
    assert_eq!(
        name_of(&btree[5]),
        Some(Value::Text("oidvector_ops".to_string()))
    );
    // Every class is one the index's *own* access method publishes — the
    // mistake a lookup ignoring `am_oid` would make.
    for (row, expected_am) in rows.iter().zip([403_u32, 405]) {
        for class in classes(row) {
            let published = opc_rows
                .iter()
                .find(|r| r[opc_oid] == class)
                .unwrap_or_else(|| panic!("indclass {class:?} has no pg_opclass row"));
            assert_eq!(published[opc_method], Value::Oid(expected_am));
        }
        assert!(row[indexrelid] != Value::Oid(0));
    }
    Ok(())
}

/// The `pg_class` columns psql's `\d` reads carry their true PostgreSQL
/// value, never a placeholder — `relchecks` counts the relation's CHECK
/// constraints, so the `0` a table without any reports is what gates psql's
/// CHECK-constraint query *off*, and `relhasrules` distinguishes a view
/// (which owns a `_RETURN` rule) from a table. `relpartbound` carries the
/// deparsed bound a leaf partition was created with, since `pg_get_expr` only
/// echoes it.
#[test]
fn pg_class_reports_describe_columns_and_partition_bounds() -> anyhow::Result<()> {
    use crabgresql_storage_api::{
        Column, PartitionBound, PartitionBoundDatum, PartitionOf, PartitionScheme,
        PartitionStrategy, TableSchema,
    };
    use crabgresql_types::PgType;

    fn plain(name: &str) -> TableSchema {
        TableSchema::new(name, vec![Column::new("a", PgType::Int4)])
    }
    // A leaf partition of `part`, bounded by one datum on each side.
    fn leaf(name: &str, from: PartitionBoundDatum, to: PartitionBoundDatum) -> TableSchema {
        let mut schema = plain(name);
        schema.partition_of = Some(PartitionOf {
            parent_namespace: "public".to_string(),
            parent_name: "part".to_string(),
            key_columns: vec![0],
            bound: PartitionBound {
                from: vec![from],
                to: vec![to],
            },
        });
        schema
    }
    // A range-partitioned parent, one leaf with a numeric bound open at the
    // top, and one leaf keyed on text (which must quote its literals).
    let mut parent = plain("part");
    parent.partition_scheme = Some(PartitionScheme {
        strategy: PartitionStrategy::Range,
        key_columns: vec![0],
    });
    let cat = SystemCatalog::with_catalog_relations("db", "owner", {
        vec![
            CatalogRelation::permanent(plain("tbl")),
            CatalogRelation::view(plain("vw"), None),
            CatalogRelation::permanent(parent.clone()),
            CatalogRelation::permanent(leaf(
                "part_hi",
                PartitionBoundDatum::Value(Value::Int4(10)),
                PartitionBoundDatum::MaxValue,
            )),
            CatalogRelation::permanent(leaf(
                "part_txt",
                PartitionBoundDatum::MinValue,
                PartitionBoundDatum::Value(Value::Text("it's".to_string())),
            )),
            CatalogRelation::permanent(leaf(
                "part_bool",
                PartitionBoundDatum::Value(Value::Bool(false)),
                PartitionBoundDatum::Value(Value::Bool(true)),
            )),
            CatalogRelation::permanent(leaf(
                "part_neg",
                PartitionBoundDatum::Value(Value::Int4(-10)),
                PartitionBoundDatum::Value(Value::Int4(0)),
            )),
        ]
    });

    let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
    assert!(rows.iter().all(|r| r.len() == schema.columns.len()));
    let relname = required(schema.column_index("relname"), "relname is missing")?;
    let cell = |name: &str, col: &str| -> anyhow::Result<Value> {
        let i = required(schema.column_index(col), col)?;
        required(
            rows.iter()
                .find(|r| r[relname] == Value::Text(name.to_string()))
                .map(|r| r[i].clone()),
            name,
        )
    };

    // No CHECK constraints, triggers, row security, typed tables, or
    // non-default tablespace exist here, and nothing has been stored out of
    // line — but each column still answers.
    for col in [
        "relchecks",
        "relhastriggers",
        "relrowsecurity",
        "relforcerowsecurity",
        "reloftype",
        "reltablespace",
        "reltoastrelid",
    ] {
        let zero = match col {
            "relchecks" => Value::Int2(0),
            "relhastriggers" | "relrowsecurity" | "relforcerowsecurity" => Value::Bool(false),
            _ => Value::Oid(0),
        };
        assert_eq!(cell("tbl", col)?, zero, "{col}");
    }
    // Only a view carries a rule; only heap-backed relations default their
    // replica identity to the primary key.
    assert_eq!(cell("tbl", "relhasrules")?, Value::Bool(false));
    assert_eq!(cell("vw", "relhasrules")?, Value::Bool(true));
    assert_eq!(cell("tbl", "relreplident")?, Value::Char(b'd'));
    assert_eq!(cell("part", "relreplident")?, Value::Char(b'd'));
    assert_eq!(cell("vw", "relreplident")?, Value::Char(b'n'));

    // A non-partition has no bound; a leaf's is the text PostgreSQL's
    // `pg_get_expr(relpartbound, oid)` prints — numbers bare, other literals
    // quoted (with embedded quotes doubled), MINVALUE/MAXVALUE as keywords.
    assert_eq!(cell("tbl", "relpartbound")?, Value::Null);
    assert_eq!(cell("part", "relpartbound")?, Value::Null);
    assert_eq!(
        cell("part_hi", "relpartbound")?,
        Value::Text("FOR VALUES FROM (10) TO (MAXVALUE)".to_string())
    );
    assert_eq!(
        cell("part_txt", "relpartbound")?,
        Value::Text("FOR VALUES FROM (MINVALUE) TO ('it''s')".to_string())
    );
    // A boolean bound is a keyword, and a negative number is quoted — both
    // as PostgreSQL prints them, and `'f'` would not even re-parse.
    assert_eq!(
        cell("part_bool", "relpartbound")?,
        Value::Text("FOR VALUES FROM (false) TO (true)".to_string())
    );
    assert_eq!(
        cell("part_neg", "relpartbound")?,
        Value::Text("FOR VALUES FROM ('-10') TO (0)".to_string())
    );

    Ok(())
}

/// A generated column reports its kind in `attgenerated`, is flagged
/// `atthasdef`, and keeps its expression in `pg_attrdef` — the same three
/// places PostgreSQL puts it, which is what lets psql's `\d` find it through the
/// join it already writes.
#[test]
fn generated_columns_are_reported_like_postgres_does() -> anyhow::Result<()> {
    use crabgresql_storage_api::{Column, GeneratedColumn, Generation, TableSchema};
    use crabgresql_types::PgType;

    let generated = |name: &str, kind: Generation, expr: &str| {
        let mut c = Column::new(name, PgType::Int4);
        c.generated = Some(GeneratedColumn {
            kind,
            expr: expr.to_string(),
        });
        c
    };
    let cat = SystemCatalog::with_relations(vec![TableSchema::new(
        "t",
        vec![
            Column::new("a", PgType::Int4),
            generated("s", Generation::Stored, "(a * 2)"),
            generated("v", Generation::Virtual, "(a + 1)"),
        ],
    )]);

    let (schema, rows) = required(
        cat.build_pg_catalog("pg_attribute"),
        "pg_attribute is missing",
    )?;
    let attname = required(schema.column_index("attname"), "attname is missing")?;
    let cell = |name: &str, col: &str| -> anyhow::Result<Value> {
        let i = required(schema.column_index(col), col)?;
        required(
            rows.iter()
                .find(|r| r[attname] == Value::Text(name.to_string()))
                .map(|r| r[i].clone()),
            name,
        )
    };
    assert_eq!(cell("a", "attgenerated")?, Value::Char(0));
    assert_eq!(cell("s", "attgenerated")?, Value::Char(b's'));
    assert_eq!(cell("v", "attgenerated")?, Value::Char(b'v'));
    assert_eq!(cell("a", "atthasdef")?, Value::Bool(false));
    assert_eq!(cell("s", "atthasdef")?, Value::Bool(true));
    assert_eq!(cell("v", "atthasdef")?, Value::Bool(true));

    let (schema, rows) = required(cat.build_pg_catalog("pg_attrdef"), "pg_attrdef is missing")?;
    let adnum = required(schema.column_index("adnum"), "adnum is missing")?;
    let adbin = required(schema.column_index("adbin"), "adbin is missing")?;
    let expr = |attnum: i16| -> Option<Value> {
        rows.iter()
            .find(|r| r[adnum] == Value::Int2(attnum))
            .map(|r| r[adbin].clone())
    };
    assert_eq!(
        expr(1),
        None,
        "an ordinary column with no default has no row"
    );
    assert_eq!(expr(2), Some(Value::Text("(a * 2)".to_string())));
    assert_eq!(expr(3), Some(Value::Text("(a + 1)".to_string())));

    Ok(())
}

/// `pg_attribute.atttypmod` is emitted in PostgreSQL's encoding, not the raw
/// modifier crabgresql stores on the column, so `format_type(atttypid,
/// atttypmod)` reproduces PG's `\d` type strings. The character types and
/// `numeric` add the four-byte varlena header; the fixed-width types do not;
/// a column with no modifier is `-1`.
#[test]
fn pg_attribute_encodes_postgres_atttypmod() -> anyhow::Result<()> {
    use crabgresql_storage_api::{Column, TableSchema};
    use crabgresql_types::{Numeric, PgType};

    let cat = SystemCatalog::with_relations(vec![TableSchema::new(
        "t",
        vec![
            Column::with_typmod("v", PgType::Varchar, 20),
            Column::with_typmod("c", PgType::Bpchar, 10),
            Column::with_typmod("b", PgType::Bit, 5),
            Column::with_typmod("vb", PgType::Varbit, 7),
            Column::with_typmod("n", PgType::Numeric, Numeric::pack_typmod(5, 2)),
            // A negative scale round trips through the signed 11-bit field.
            Column::with_typmod("nn", PgType::Numeric, Numeric::pack_typmod(4, -2)),
            Column::new("i", PgType::Int4),
        ],
    )]);
    let (schema, rows) = required(
        cat.build_pg_catalog("pg_attribute"),
        "pg_attribute is missing",
    )?;
    assert!(rows.iter().all(|r| r.len() == schema.columns.len()));
    let attname = required(schema.column_index("attname"), "attname is missing")?;
    let cell = |name: &str, col: &str| -> anyhow::Result<Value> {
        let i = required(schema.column_index(col), col)?;
        required(
            rows.iter()
                .find(|r| r[attname] == Value::Text(name.to_string()))
                .map(|r| r[i].clone()),
            name,
        )
    };

    // varchar(20) / character(10) reserve VARHDRSZ; bit(5) / varbit(7) do not.
    assert_eq!(cell("v", "atttypmod")?, Value::Int4(24));
    assert_eq!(cell("c", "atttypmod")?, Value::Int4(14));
    assert_eq!(cell("b", "atttypmod")?, Value::Int4(5));
    assert_eq!(cell("vb", "atttypmod")?, Value::Int4(7));
    // The values PostgreSQL 18.4 stores for `numeric(5,2)`/`numeric(4,-2)`.
    assert_eq!(cell("n", "atttypmod")?, Value::Int4(327686));
    assert_eq!(cell("nn", "atttypmod")?, Value::Int4(264194));
    assert_eq!(cell("i", "atttypmod")?, Value::Int4(-1));
    // An ordinary column carries PG's "neither" spelling for both: `\0` rather
    // than NULL — a `"char"` that prints as the empty string, which psql
    // projects directly.
    //
    // TODO: report attidentity once identity columns can be declared.
    assert_eq!(cell("i", "attidentity")?, Value::Char(0));
    assert_eq!(cell("i", "attgenerated")?, Value::Char(0));

    Ok(())
}

/// Building `pg_attribute` must not panic on a length that would overflow
/// PostgreSQL's `n + VARHDRSZ` encoding. DDL rejects such a length, so this
/// is only reachable from a data directory that already holds one — where a
/// panic would make the catalog permanently unreadable rather than merely
/// misreport a column.
#[test]
fn oversized_typmod_saturates_instead_of_panicking() -> anyhow::Result<()> {
    use crabgresql_storage_api::{Column, TableSchema};
    use crabgresql_types::PgType;

    let cat = SystemCatalog::with_relations(vec![TableSchema::new(
        "t",
        vec![Column::with_typmod("v", PgType::Varchar, i32::MAX)],
    )]);
    let (schema, rows) = required(
        cat.build_pg_catalog("pg_attribute"),
        "pg_attribute is missing",
    )?;
    let i = required(schema.column_index("atttypmod"), "atttypmod is missing")?;
    assert_eq!(rows[0][i], Value::Int4(i32::MAX));

    Ok(())
}

/// The registry is sorted by `(namespace, name)` with unique names, which is
/// what `registry::lookup`'s binary search assumes. Out of order it would not
/// error — it would silently fail to find a relation that is right there.
#[test]
fn the_registry_is_sorted_and_its_names_are_unique() {
    let keys: Vec<_> = registry::CATALOG_RELATIONS
        .iter()
        .map(|def| (def.namespace, def.name))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(keys, sorted, "CATALOG_RELATIONS must be sorted and unique");

    // ...and the search really does find every entry, in both directions.
    for def in registry::CATALOG_RELATIONS {
        assert!(registry::lookup(def.namespace, def.name).is_some());
    }
    assert!(registry::lookup(CatalogNamespace::PgCatalog, "no_such_catalog").is_none());
}

/// Every `pg_catalog` relation has a distinct, non-zero OID that round-trips,
/// and none of them lands in a band some other OID space owns.
///
/// A hand-typed OID is the one thing in the registry no compiler checks. A
/// digit slipped into a synthetic band would make `'pg_class'::regclass`
/// name a user relation — or two catalogs answer to the same cast.
#[test]
fn catalog_oids_are_unique_and_outside_every_synthetic_band() {
    let mut seen = std::collections::HashSet::new();
    for def in registry::CATALOG_RELATIONS {
        if def.namespace != CatalogNamespace::PgCatalog {
            // The information_schema entries have no OID to check; the TODO on
            // `CatalogRelDef::oid` says why, and this pins the sentinel so it
            // cannot be read as a real assignment.
            assert_eq!(def.oid, 0, "{} must not claim an OID yet", def.name);
            continue;
        }
        assert_ne!(def.oid, 0, "{} has no OID", def.name);
        assert!(seen.insert(def.oid), "OID {} is claimed twice", def.oid);
        assert_eq!(builtin_relation_oid(def.name), Some(def.oid));
        assert_eq!(builtin_relation_name(def.oid), Some(def.name));

        assert!(
            def.oid < FIRST_REL_OID,
            "{} sits in the synthetic relation band",
            def.name
        );
        assert!(
            def.oid < oids::FIRST_ENUM_OID,
            "{} sits in the enum-label band",
            def.name
        );
        // Codegen numbers `pg_cast` rows from 10000 upward with no reserved
        // ceiling, and the initdb-created views this registry lists sit above
        // that start, in the 12000s — no numeric bound separates the two, so
        // this asks the generated rows themselves.
        assert!(
            !PG_CAST_ROWS.iter().any(|row| row.oid == def.oid),
            "{} shares its OID with a generated pg_cast row",
            def.name
        );
    }
    assert_eq!(builtin_relation_oid("no_such_catalog"), None);
    assert_eq!(builtin_relation_name(0), None);
}

/// Each entry's `schema` really is the relation the entry names. A copy-paste
/// in the registry would otherwise serve `pg_shadow`'s rows under
/// `pg_user`'s name, and every column would still line up.
#[test]
fn each_entry_serves_the_relation_it_names() {
    for def in registry::CATALOG_RELATIONS {
        let schema = (def.schema)();
        assert_eq!(schema.name, def.name);
        let namespace = match def.namespace {
            CatalogNamespace::PgCatalog => "pg_catalog",
            CatalogNamespace::InformationSchema => "information_schema",
        };
        assert_eq!(schema.namespace, namespace, "{}", def.name);
    }
}

/// A snapshot holding one of everything the catalogs reflect, so the width
/// check below exercises every row-building path rather than the empty one.
fn wide_fixture() -> SystemCatalog {
    use crabgresql_storage_api::{
        CheckConstraint, IndexKey, IndexMethod, PartitionBound, PartitionBoundDatum, PartitionOf,
        PartitionScheme, PartitionStrategy,
    };

    let mut table = TableSchema::new(
        "tbl",
        vec![
            Column::new("id", PgType::Int4),
            Column::new("body", PgType::Text),
        ],
    );
    table.columns[0].not_null_constraint = Some("tbl_id_not_null".to_string());
    table.columns[1].default = Some("'x'::text".to_string());
    table.checks = vec![CheckConstraint {
        name: "tbl_id_check".to_string(),
        expr: "(id > 0)".to_string(),
        columns: vec![0],
        validated: true,
        islocal: true,
        inhcount: 0,
    }];
    let mut indexed = CatalogRelation::permanent(table.clone());
    indexed.indexes = vec![IndexMetadata {
        name: "tbl_pkey".to_string(),
        method: IndexMethod::BTree,
        keys: vec![IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        }],
        unique: true,
        nulls_distinct: true,
        constraint: Some(IndexConstraint::PrimaryKey),
    }];
    indexed.toast = Some(RelStats {
        relpages: 3,
        reltuples: 0.0,
        analyzed: false,
        curpages: Some(3),
        columns: std::sync::Arc::from([]),
    });

    let mut parent = TableSchema::new("part", vec![Column::new("k", PgType::Int4)]);
    parent.partition_scheme = Some(PartitionScheme {
        strategy: PartitionStrategy::Range,
        key_columns: vec![0],
    });
    let mut leaf = TableSchema::new("part_1", vec![Column::new("k", PgType::Int4)]);
    leaf.partition_of = Some(PartitionOf {
        parent_namespace: "public".to_string(),
        parent_name: "part".to_string(),
        key_columns: vec![0],
        bound: PartitionBound {
            from: vec![PartitionBoundDatum::Value(Value::Int4(0))],
            to: vec![PartitionBoundDatum::Value(Value::Int4(10))],
        },
    });

    SystemCatalog::from_source(Arc::new(
        StaticSource::new(vec![
            indexed,
            CatalogRelation::permanent(parent),
            CatalogRelation::permanent(leaf),
            CatalogRelation::view(
                TableSchema::new("vw", vec![Column::new("a", PgType::Int4)]),
                Some("SELECT a FROM tbl".to_string()),
            ),
            CatalogRelation::temporary(
                TableSchema::new("tmp", vec![Column::new("a", PgType::Int4)]),
                "pg_temp_3",
            ),
            CatalogRelation::sequence(
                "sq",
                "app",
                CatalogSequence {
                    type_oid: PgType::Int8.oid(),
                    start: 1,
                    increment: 1,
                    min: 1,
                    max: i64::MAX,
                    cache: 1,
                    cycle: false,
                    last_value: Some(1),
                },
            ),
        ])
        .database("db")
        .owner("alice")
        .schemas(vec![("app".to_string(), 16_400)])
        .user_types(vec![CatalogUserType {
            oid: 16_500,
            name: "mood".to_string(),
            enum_labels: Some(vec!["sad".to_string(), "ok".to_string()]),
        }])
        .routines(vec![CatalogRoutine {
            oid: 16_600,
            name: "f".to_string(),
            namespace: "public".to_string(),
            kind: 'f',
            lang: PLPGSQL_LANG_OID,
            arg_types: vec![PgType::Int4.oid()],
            all_arg_types: Vec::new(),
            arg_modes: Vec::new(),
            arg_names: vec!["a".to_string()],
            ret_type: PgType::Int4.oid(),
            retset: false,
            volatile: 'v',
            strict: false,
            secdef: false,
            src: "begin return a; end".to_string(),
        }])
        .cursors(vec![CatalogCursor {
            name: "c".to_string(),
            statement: "DECLARE c CURSOR FOR SELECT 1".to_string(),
            is_holdable: false,
            is_binary: false,
            is_scrollable: false,
            creation_time: 0,
        }])
        .locks(vec![CatalogLock {
            target: CatalogLockTarget::VirtualXid,
            virtualtransaction: "4/7".to_string(),
            pid: 4,
            mode: "ExclusiveLock",
            granted: true,
            fastpath: true,
            waitstart: None,
        }])
        .settings(vec![CatalogSetting {
            name: "TimeZone",
            setting: "UTC".to_string(),
            unit: None,
            category: "Client Connection Defaults",
            short_desc: "Sets the time zone for displaying and interpreting time stamps.",
            extra_desc: None,
            context: "user",
            vartype: "string",
            source: "default",
            min_val: None,
            max_val: None,
            enumvals: None,
            boot_val: "GMT",
            reset_val: "UTC".to_string(),
        }]),
    ))
}

/// Every relation in the registry builds rows exactly as wide as the schema
/// it publishes, against a snapshot that populates all of them.
///
/// A row one column short is not a build error: the client reads the
/// remaining columns shifted, or the scan panics indexing past the end. The
/// check used to be spot-applied to the two relations whose row builders had
/// several paths; run across the registry it also fails for a relation added
/// with a column in the schema and not in the rows.
#[test]
fn every_relation_builds_rows_as_wide_as_its_schema() {
    let cat = wide_fixture();
    for def in registry::CATALOG_RELATIONS {
        let schema = (def.schema)();
        let rows = (def.rows)(&cat);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(
                row.len(),
                schema.columns.len(),
                "{} row {i} is {} wide, schema is {}",
                def.name,
                row.len(),
                schema.columns.len()
            );
        }
    }
}

/// The fixture is only a width check if the relations it feeds are non-empty.
/// Without this, dropping a field from `wide_fixture` would quietly turn the
/// test above into a loop over nothing.
#[test]
fn the_wide_fixture_populates_every_derived_relation() {
    let cat = wide_fixture();
    for name in [
        "pg_class",
        "pg_attribute",
        "pg_attrdef",
        "pg_constraint",
        "pg_index",
        "pg_inherits",
        "pg_partitioned_table",
        "pg_sequence",
        "pg_namespace",
        "pg_enum",
        "pg_cursors",
        "pg_locks",
        "pg_settings",
        "pg_proc",
        "pg_type",
        // The per-relkind views over the same snapshot. Each one is fed by a
        // different arm of the fixture — a table, a view, a sequence, an index —
        // so an empty answer here means that arm stopped reaching the view.
        "pg_tables",
        "pg_views",
        "pg_sequences",
        "pg_indexes",
        "pg_extension",
        "pg_description",
        "pg_rewrite",
    ] {
        let def = registry::lookup(CatalogNamespace::PgCatalog, name).expect(name);
        assert!(!(def.rows)(&cat).is_empty(), "{name} built no rows");
    }
    for name in ["schemata", "tables", "columns"] {
        let def = registry::lookup(CatalogNamespace::InformationSchema, name).expect(name);
        assert!(!(def.rows)(&cat).is_empty(), "{name} built no rows");
    }
}

/// `pg_get_userbyid`'s and `pg_table_is_visible`'s backing lookups agree with
/// the `pg_class` rows built from the same snapshot: the `relowner` every row
/// reports resolves to a name, and every row's OID resolves to the namespace
/// that row reports.
#[test]
fn catalog_lookups_agree_with_pg_class_rows() -> anyhow::Result<()> {
    let cat = SystemCatalog::from_source(Arc::new(
        StaticSource::new(vec![CatalogRelation::permanent(TableSchema::in_namespace(
            "t",
            "app",
            vec![Column::new("a", PgType::Int4)],
        ))])
        .database("db")
        .owner("alice")
        .schemas(vec![("app".to_string(), 16_000)]),
    ));
    let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
    let oid = required(schema.column_index("oid"), "oid column is missing")?;
    let relowner = required(
        schema.column_index("relowner"),
        "relowner column is missing",
    )?;
    let relnamespace = required(
        schema.column_index("relnamespace"),
        "relnamespace column is missing",
    )?;
    let row = required(rows.first(), "expected one pg_class row")?;

    // The owner OID pg_class reports is the one `pg_get_userbyid` resolves.
    // Asserted against the constant, not a literal, so moving the bootstrap
    // OID cannot leave the row and the lookup disagreeing.
    assert_eq!(row[relowner], Value::Oid(oids::BOOTSTRAP_ROLE_OID));
    assert_eq!(cat.role_name(oids::BOOTSTRAP_ROLE_OID), Some("alice"));
    assert_eq!(cat.role_name(oids::BOOTSTRAP_ROLE_OID + 1), None);

    // Every other owner column reports the same role, so `pg_get_userbyid`
    // resolves them all rather than printing `unknown (OID=n)` for some.
    for relation in ["pg_type", "pg_collation", "pg_namespace"] {
        let (s, r) = required(cat.build_pg_catalog(relation), relation)?;
        let owner = required(
            s.columns.iter().position(|c| c.name.ends_with("owner")),
            "an owner column",
        )?;
        for row in &r {
            let Value::Oid(o) = row[owner] else {
                anyhow::bail!("{relation} owner column was not an OID");
            };
            assert!(
                cat.role_name(o).is_some(),
                "{relation} owner OID {o} does not resolve to a role name"
            );
        }
    }

    // ... and the namespace it reports is the one visibility is decided on.
    let Value::Oid(rel_oid) = row[oid] else {
        anyhow::bail!("pg_class.oid was not an OID");
    };
    assert_eq!(cat.relation_ref(rel_oid), Some(("app", "t")));
    assert_eq!(cat.relation_oid_in("app", "t"), Some(rel_oid));
    assert_eq!(
        cat.namespace_oids().get("app").copied().map(Value::Oid),
        Some(row[relnamespace].clone())
    );
    // An OID no relation has resolves to nothing, so the function is NULL —
    // both above the assigned range and below the synthetic floor.
    assert_eq!(cat.relation_ref(rel_oid + 1_000), None);
    assert_eq!(cat.relation_ref(1), None);

    Ok(())
}

#[test]
fn unknown_relation_is_not_found() {
    let cat = SystemCatalog::new();
    assert!(cat.open_table("pg_type").is_ok());
    assert!(cat.open_table("pg_namespace").is_ok());
    assert!(cat.open_table("pg_cast").is_ok());
    assert!(cat.open_table("pg_am").is_ok());
    assert!(matches!(
        cat.open_table("pg_nonexistent"),
        Err(StorageError::TableNotFound(_))
    ));
}

#[test]
fn information_schema_reflects_relation_metadata() -> anyhow::Result<()> {
    use crabgresql_storage_api::{Column, TableSchema};
    use crabgresql_types::PgType;

    let cat = SystemCatalog::with_catalog_relations("appdb", "appuser", {
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

/// `relpages`/`reltuples` follow PostgreSQL's rule that they are written only
/// by `ANALYZE`, and both of `pg_class_rows`' row-building paths (relations,
/// then indexes) stay as wide as the schema.
#[test]
fn pg_class_size_columns_report_the_never_analyzed_sentinel() -> anyhow::Result<()> {
    use crabgresql_storage_api::{Column, IndexKey, IndexMethod, RelStats, TableSchema};
    use crabgresql_types::PgType;

    let table = TableSchema::new("tbl", vec![Column::new("a", PgType::Int4)]);
    let index = IndexMetadata {
        name: "tbl_a_idx".to_string(),
        method: IndexMethod::BTree,
        keys: vec![IndexKey {
            column: 0,
            descending: false,
            nulls_first: false,
        }],
        unique: false,
        nulls_distinct: true,
        constraint: None,
    };
    // `analyzed` stands in for a relation ANALYZE has measured; the others
    // have not been analyzed and must report the sentinel.
    let analyzed = RelStats::exact(1234, &table);
    let cat = SystemCatalog::with_catalog_relations("db", "owner", {
        let mut measured = CatalogRelation::permanent(TableSchema::new(
            "measured",
            vec![Column::new("a", PgType::Int4)],
        ));
        measured.stats = analyzed.clone();
        let mut indexed = CatalogRelation::permanent(table.clone());
        indexed.indexes = vec![index.clone()];
        vec![
            measured,
            indexed,
            CatalogRelation::view(
                TableSchema::new("vw", vec![Column::new("a", PgType::Int4)]),
                Some("SELECT a FROM tbl".to_string()),
            ),
            CatalogRelation::sequence(
                "sq",
                "public",
                CatalogSequence {
                    type_oid: PgType::Int8.oid(),
                    start: 1,
                    increment: 1,
                    min: 1,
                    max: i64::MAX,
                    cache: 1,
                    cycle: false,
                    last_value: Some(1),
                },
            ),
        ]
    });

    let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
    // The index row is built by a separate path from the relation rows;
    // both must match the schema width or a client reads shifted columns.
    assert_eq!(rows.len(), 5, "four relations plus one index");
    assert!(rows.iter().all(|r| r.len() == schema.columns.len()));

    let relname = required(schema.column_index("relname"), "relname is missing")?;
    let cell = |name: &str, col: &str| -> anyhow::Result<Value> {
        let i = required(schema.column_index(col), col)?;
        required(
            rows.iter()
                .find(|r| r[relname] == Value::Text(name.to_string()))
                .map(|r| r[i].clone()),
            name,
        )
    };

    // Never analyzed: no pages and the `-1` unknown sentinel — NOT zero,
    // which would claim the relation is known to be empty.
    for name in ["tbl", "vw", "tbl_a_idx"] {
        assert_eq!(cell(name, "relpages")?, Value::Int4(0), "{name}");
        assert_eq!(cell(name, "reltuples")?, Value::Float4(-1.0), "{name}");
    }
    // Analyzed: the measured count, reported as-is.
    assert_eq!(cell("measured", "reltuples")?, Value::Float4(1234.0));
    assert!(matches!(cell("measured", "relpages")?, Value::Int4(p) if p > 0));
    // A sequence is one page holding one row from the moment it is created.
    assert_eq!(cell("sq", "relpages")?, Value::Int4(1));
    assert_eq!(cell("sq", "reltuples")?, Value::Float4(1.0));
    // No visibility map is kept, so nothing is ever all-visible.
    assert_eq!(cell("measured", "relallvisible")?, Value::Int4(0));

    Ok(())
}

#[test]
fn a_toast_relation_is_published_and_its_parent_points_at_it() -> anyhow::Result<()> {
    // `reltoastrelid` is a foreign key into `pg_class.oid`, so publishing the
    // row is what makes a non-zero value safe rather than a dangling
    // reference. A table that has never stored anything out of line keeps 0,
    // which is what PostgreSQL reports for a table with no TOAST relation.
    fn plain(name: &str) -> TableSchema {
        TableSchema::new(name, vec![Column::new("id", PgType::Int4)])
    }
    let cat = SystemCatalog::with_catalog_relations("db", "owner", {
        let mut toasted = CatalogRelation::permanent(plain("toasted"));
        toasted.toast = Some(RelStats {
            relpages: 7,
            reltuples: 0.0,
            analyzed: false,
            curpages: Some(7),
            columns: std::sync::Arc::from([]),
        });
        vec![CatalogRelation::permanent(plain("bare")), toasted]
    });

    let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
    let col = |name: &str| required(schema.column_index(name), name);
    let (relname, oid, toastrel) = (col("relname")?, col("oid")?, col("reltoastrelid")?);
    let row = |name: &str| {
        rows.iter()
            .find(|r| r[relname] == Value::Text(name.to_string()))
            .cloned()
    };

    let toasted = required(row("toasted"), "toasted")?;
    let Value::Oid(toast_oid) = toasted[toastrel] else {
        anyhow::bail!("reltoastrelid is not an OID");
    };
    assert_ne!(toast_oid, 0, "a relation with out-of-line storage names it");
    assert_eq!(
        required(row("bare"), "bare")?[toastrel],
        Value::Oid(0),
        "a relation with none must not borrow its neighbour's"
    );

    // The OID resolves to a real row, in `pg_toast`, named after its parent.
    let toast_row = required(
        rows.iter()
            .find(|r| r[oid] == Value::Oid(toast_oid))
            .cloned(),
        "the toast relation has no pg_class row",
    )?;
    let Value::Oid(parent_oid) = toasted[oid] else {
        anyhow::bail!("oid is not an OID");
    };
    assert_eq!(
        toast_row[relname],
        Value::Text(format!("pg_toast_{parent_oid}"))
    );
    assert_eq!(toast_row[col("relkind")?], Value::Char(b't'));
    assert_eq!(toast_row[col("relnamespace")?], Value::Oid(99));
    assert_eq!(toast_row[col("relpages")?], Value::Int4(7));
    // We chain chunks by ctid rather than indexing them, so claiming an index
    // would be the dangling reference this row exists to avoid.
    assert_eq!(toast_row[col("relhasindex")?], Value::Bool(false));

    // Every OID is distinct: the toast block sits after the index block, so
    // it can neither collide with nor shift an existing assignment.
    let mut oids: Vec<&Value> = rows.iter().map(|r| &r[oid]).collect();
    let total = oids.len();
    oids.sort_by_key(|v| match v {
        Value::Oid(o) => *o,
        _ => 0,
    });
    oids.dedup();
    assert_eq!(oids.len(), total, "pg_class OIDs must be unique");

    // Its columns join, so `relnatts` is not a claim without rows behind it.
    let (aschema, arows) = required(
        cat.build_pg_catalog("pg_attribute"),
        "pg_attribute is missing",
    )?;
    let attrelid = required(aschema.column_index("attrelid"), "attrelid")?;
    let attname = required(aschema.column_index("attname"), "attname")?;
    let names: Vec<String> = arows
        .iter()
        .filter(|r| r[attrelid] == Value::Oid(toast_oid))
        .map(|r| match &r[attname] {
            Value::Text(s) => s.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["chunk_id", "chunk_seq", "chunk_data"]);

    // A toast relation is not a user relation: it must not be reachable by an
    // unqualified name, which is why it never enters `live_relations`.
    assert_eq!(cat.relation_oid_in("public", "pg_toast_1"), None);
    assert_eq!(
        cat.relation_ref(toast_oid),
        Some(("pg_toast", format!("pg_toast_{parent_oid}").as_str()))
    );
    Ok(())
}

/// `pg_stats` and `pg_statistic` describe the same measurement two ways: the
/// view names the columns a reader knows, the catalog packs them into the
/// generic slots PostgreSQL defines. A relation nothing measured appears in
/// neither.
#[test]
fn statistics_relations_report_what_analyze_measured() -> anyhow::Result<()> {
    use crabgresql_storage_api::{ColStats, Column, RelStats, TableSchema};
    use crabgresql_types::PgType;

    let schema = TableSchema::new(
        "measured",
        vec![
            Column::new("g", PgType::Int4),
            Column::new("t", PgType::Text),
        ],
    );
    let cat = SystemCatalog::with_catalog_relations("db", "owner", {
        let mut measured = CatalogRelation::permanent(schema.clone());
        measured.stats = RelStats {
            relpages: 4,
            reltuples: 1000.0,
            analyzed: true,
            curpages: Some(4),
            columns: std::sync::Arc::from([
                // A skewed integer column: two common values and a histogram of
                // what is left.
                ColStats {
                    null_frac: 0.1,
                    avg_width: 4,
                    n_distinct: 5.0,
                    mcv: vec![(Value::Int4(1), 0.5), (Value::Int4(2), 0.25)],
                    histogram: vec![Value::Int4(10), Value::Int4(20)],
                    correlation: 0.5,
                },
                // A column with nothing common enough to list.
                ColStats {
                    null_frac: 0.0,
                    avg_width: 12,
                    n_distinct: -1.0,
                    mcv: Vec::new(),
                    histogram: vec![Value::Text("a".into()), Value::Text("b".into())],
                    correlation: -1.0,
                },
            ]),
        };
        vec![
            measured,
            CatalogRelation::permanent(TableSchema::new(
                "unmeasured",
                vec![Column::new("a", PgType::Int4)],
            )),
        ]
    });

    let (vschema, vrows) = required(cat.build_pg_catalog("pg_stats"), "pg_stats is missing")?;
    let at = |row: &[Value], col: &str| -> Value {
        row[vschema.column_index(col).expect("column exists")].clone()
    };
    // Only the analyzed relation's columns, in attnum order.
    let described: Vec<Value> = vrows.iter().map(|r| at(r, "attname")).collect();
    assert_eq!(
        described,
        vec![Value::Text("g".into()), Value::Text("t".into())]
    );
    assert_eq!(at(&vrows[0], "tablename"), Value::Text("measured".into()));
    assert_eq!(at(&vrows[0], "null_frac"), Value::Float4(0.1));
    assert_eq!(at(&vrows[0], "n_distinct"), Value::Float4(5.0));
    assert_eq!(
        at(&vrows[0], "most_common_vals"),
        Value::Text("{1,2}".into())
    );
    assert_eq!(
        at(&vrows[0], "most_common_freqs"),
        Value::Text("{0.5,0.25}".into())
    );
    assert_eq!(
        at(&vrows[0], "histogram_bounds"),
        Value::Text("{10,20}".into())
    );
    assert_eq!(at(&vrows[0], "correlation"), Value::Float4(0.5));
    // No MCV list is NULL, not an empty array — the distinction PostgreSQL draws.
    assert_eq!(at(&vrows[1], "most_common_vals"), Value::Null);
    assert_eq!(
        at(&vrows[1], "histogram_bounds"),
        Value::Text("{a,b}".into())
    );
    // Never collected here, and NULL is what PostgreSQL shows for them.
    assert_eq!(at(&vrows[0], "most_common_elems"), Value::Null);

    let (cschema, crows) = required(
        cat.build_pg_catalog("pg_statistic"),
        "pg_statistic is missing",
    )?;
    let raw = |row: &[Value], col: &str| -> Value {
        row[cschema.column_index(col).expect("column exists")].clone()
    };
    assert_eq!(crows.len(), 2);
    assert_eq!(raw(&crows[0], "staattnum"), Value::Int2(1));
    // Slots fill in order: MCV, then histogram, then correlation.
    assert_eq!(raw(&crows[0], "stakind1"), Value::Int2(1));
    assert_eq!(raw(&crows[0], "stakind2"), Value::Int2(2));
    assert_eq!(raw(&crows[0], "stakind3"), Value::Int2(3));
    assert_eq!(raw(&crows[0], "stakind4"), Value::Int2(0));
    assert_eq!(raw(&crows[0], "stavalues1"), Value::Text("{1,2}".into()));
    assert_eq!(
        raw(&crows[0], "stanumbers1"),
        Value::Text("{0.5,0.25}".into())
    );
    assert_eq!(raw(&crows[0], "stavalues2"), Value::Text("{10,20}".into()));
    assert_eq!(raw(&crows[0], "stanumbers3"), Value::Text("{0.5}".into()));
    // The second column has no MCV list, so the histogram takes slot 1.
    assert_eq!(raw(&crows[1], "stakind1"), Value::Int2(2));
    assert_eq!(raw(&crows[1], "stavalues1"), Value::Text("{a,b}".into()));
    Ok(())
}

/// Nothing may describe an object that is not there. Every `pg_description`
/// row is checked against the very relation its `classoid` names, built from
/// the same snapshot — the guard that keeps a `.dat` resync from publishing a
/// comment on a function this build does not have.
#[test]
fn every_description_names_a_row_that_exists() -> anyhow::Result<()> {
    let cat = wide_fixture();
    let (schema, rows) = required(
        cat.build_pg_catalog("pg_description"),
        "pg_description is missing",
    )?;
    let at =
        |row: &[Value], col: &str| row[schema.column_index(col).expect("column exists")].clone();
    assert!(!rows.is_empty());
    // The described catalogs are built once each: `pg_proc` alone has hundreds
    // of rows, and rebuilding it per description would be quadratic.
    let mut described: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    for row in &rows {
        let (Value::Oid(classoid), Value::Oid(objoid)) = (at(row, "classoid"), at(row, "objoid"))
        else {
            anyhow::bail!("pg_description row {row:?} has a non-OID key");
        };
        // Bootstrap data describes whole objects only, as PostgreSQL's does.
        assert_eq!(at(row, "objsubid"), Value::Int4(0));
        let name = required(
            builtin_relation_name(classoid),
            "a description's classoid names no pg_catalog relation",
        )?;
        let oids = match described.entry(classoid) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let (cschema, crows) = required(cat.build_pg_catalog(name), name)?;
                let oid = required(cschema.column_index("oid"), "oid column is missing")?;
                e.insert(
                    crows
                        .iter()
                        .filter_map(|r| match r[oid] {
                            Value::Oid(oid) => Some(oid),
                            _ => None,
                        })
                        .collect(),
                )
            }
        };
        assert!(
            oids.contains(&objoid),
            "{name} has no row {objoid}, but something describes one"
        );
    }
    Ok(())
}

/// The census, per catalog. A `.dat` resync that adds or drops a built-in shows
/// up here rather than silently.
#[test]
fn the_bootstrap_descriptions_cover_five_catalogs_and_the_extension() -> anyhow::Result<()> {
    let cat = wide_fixture();
    let (schema, rows) = required(
        cat.build_pg_catalog("pg_description"),
        "pg_description is missing",
    )?;
    let classoid = required(
        schema.column_index("classoid"),
        "classoid column is missing",
    )?;
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for row in &rows {
        let Value::Oid(oid) = row[classoid] else {
            anyhow::bail!("a classoid is not an OID");
        };
        *counts
            .entry(required(builtin_relation_name(oid), "unknown classoid")?)
            .or_default() += 1;
    }
    assert_eq!(
        counts,
        std::collections::BTreeMap::from([
            // The seven upstream access methods plus crabgresql's own two.
            ("pg_am", 9),
            ("pg_extension", 1),
            // Three from the `.dat`, plus `plpgsql`.
            ("pg_language", 4),
            ("pg_namespace", 3),
            ("pg_proc", 515),
            ("pg_type", 109),
        ])
    );
    Ok(())
}

/// `pg_namespace.dat` describes four schemas and this build serves three, so
/// one generated description has to be dropped — the case that makes the
/// `PUBLISHED` filter more than decoration.
#[test]
fn a_description_of_a_schema_this_build_lacks_is_dropped() -> anyhow::Result<()> {
    let generated = PG_DESCRIPTION_ROWS
        .iter()
        .filter(|row| row.catalog == "pg_namespace")
        .count();
    assert_eq!(generated, 4);
    let ns = required(
        builtin_relation_oid("pg_namespace"),
        "pg_namespace is not served",
    )?;
    let cat = wide_fixture();
    let (schema, rows) = required(
        cat.build_pg_catalog("pg_description"),
        "pg_description is missing",
    )?;
    let classoid = required(
        schema.column_index("classoid"),
        "classoid column is missing",
    )?;
    let served = rows
        .iter()
        .filter(|row| row[classoid] == Value::Oid(ns))
        .count();
    assert_eq!(served, 3);
    Ok(())
}

/// `obj_description` and a `SELECT` from `pg_description` read one list, so
/// they cannot disagree — and the lookup answers nothing rather than raising
/// for a class, an OID or a column number nobody described.
#[test]
fn the_description_lookup_matches_the_published_rows() -> anyhow::Result<()> {
    let types = required(builtin_relation_oid("pg_type"), "pg_type is not served")?;
    assert_eq!(
        object_description(types, 23, 0),
        Some("-2 billion to 2 billion integer, 4-byte storage")
    );
    // No column comments exist, here or in PostgreSQL's bootstrap data.
    assert_eq!(object_description(types, 23, 1), None);
    // A `classoid` of 0 is what an unresolvable catalog name comes to.
    assert_eq!(object_description(0, 23, 0), None);
    assert_eq!(object_description(types, 999_999, 0), None);
    assert_eq!(
        object_descriptions_any_class(23, 0),
        vec!["-2 billion to 2 billion integer, 4-byte storage"]
    );
    assert!(object_descriptions_any_class(999_999, 0).is_empty());
    Ok(())
}

/// `pg_locks` renders each lock target into the identity column PostgreSQL
/// fills for it and leaves the others NULL, numbering a relation lock from the
/// same snapshot it is rendering.
///
/// The three targets share one row builder, so the risk is a column that stays
/// filled from the wrong arm — a `relation` OID surviving onto a `virtualxid`
/// row would name a relation the lock has nothing to do with.
#[test]
fn pg_locks_fills_one_identity_column_per_lock_target() -> anyhow::Result<()> {
    let lock = |target| CatalogLock {
        target,
        virtualtransaction: "4/7".to_string(),
        pid: 4,
        mode: "ExclusiveLock",
        granted: true,
        fastpath: true,
        waitstart: None,
    };
    let relation = |namespace: &str, name: &str| CatalogLockTarget::Relation {
        namespace: namespace.to_string(),
        name: name.to_string(),
    };
    let cat = SystemCatalog::from_source(Arc::new(
        StaticSource::new(vec![CatalogRelation::permanent(TableSchema::in_namespace(
            "t",
            "app",
            vec![Column::new("a", PgType::Int4)],
        ))])
        .schemas(vec![("app".to_string(), 16_400)])
        .locks(vec![
            lock(CatalogLockTarget::VirtualXid),
            lock(CatalogLockTarget::TransactionId(31)),
            CatalogLock {
                mode: "AccessExclusiveLock",
                granted: false,
                fastpath: false,
                waitstart: Some(42),
                ..lock(relation("app", "t"))
            },
            lock(relation("pg_catalog", "pg_locks")),
            // A relation this snapshot cannot number: dropped rather than
            // reported under an OID that names nothing.
            lock(relation("app", "gone")),
        ]),
    ));
    let (schema, rows) = required(cat.build_pg_catalog("pg_locks"), "pg_locks is missing")?;
    let col = |row: &[Value], name: &str| -> anyhow::Result<Value> {
        Ok(row[required(schema.column_index(name), name)?].clone())
    };
    let locks_oid = required(builtin_relation_oid("pg_locks"), "pg_locks has no OID")?;
    assert_eq!(locks_oid, 12_073);
    assert_eq!(rows.len(), 4);

    assert_eq!(col(&rows[0], "locktype")?, Value::Text("virtualxid".into()));
    assert_eq!(col(&rows[0], "virtualxid")?, Value::Text("4/7".into()));
    assert_eq!(col(&rows[0], "transactionid")?, Value::Null);
    assert_eq!(col(&rows[0], "relation")?, Value::Null);
    // Cluster-wide, so no database — PostgreSQL leaves it NULL too.
    assert_eq!(col(&rows[0], "database")?, Value::Null);
    assert_eq!(col(&rows[0], "pid")?, Value::Int4(4));

    assert_eq!(
        col(&rows[1], "locktype")?,
        Value::Text("transactionid".into())
    );
    assert_eq!(col(&rows[1], "transactionid")?, Value::Xid(31));
    assert_eq!(col(&rows[1], "virtualxid")?, Value::Null);

    assert_eq!(col(&rows[2], "locktype")?, Value::Text("relation".into()));
    assert_eq!(
        col(&rows[2], "relation")?,
        Value::Oid(required(
            cat.relation_oid_in("app", "t"),
            "app.t has no OID"
        )?)
    );
    assert_eq!(col(&rows[2], "database")?, Value::Oid(oids::DATABASE_OID));
    assert_eq!(col(&rows[2], "granted")?, Value::Bool(false));
    assert_eq!(col(&rows[2], "waitstart")?, Value::TimestampTz(42));

    // A `pg_catalog` relation carries PostgreSQL's own OID, not a snapshot one.
    assert_eq!(col(&rows[3], "relation")?, Value::Oid(locks_oid));

    for row in &rows {
        for name in ["page", "tuple", "classid", "objid", "objsubid"] {
            assert_eq!(col(row, name)?, Value::Null, "{name} should be NULL");
        }
    }
    Ok(())
}

/// A snapshot with no session behind it reports no locks at all, rather than a
/// scan-lock row attributed to a holder that does not exist.
#[test]
fn pg_locks_is_empty_without_a_session() -> anyhow::Result<()> {
    let (_, rows) = required(
        SystemCatalog::new().build_pg_catalog("pg_locks"),
        "pg_locks is missing",
    )?;
    assert!(rows.is_empty());
    Ok(())
}
