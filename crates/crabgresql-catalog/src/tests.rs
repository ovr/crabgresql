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
    // alpha has two columns, in declared order, tied to alpha's OID — plus the
    // six system attributes every relation carries, which sit at negative
    // `attnum` and are filtered out here exactly as a client's column list
    // filters them.
    let all_alpha: Vec<_> = attr
        .iter()
        .filter(|r| r[arel] == Value::Oid(FIRST_REL_OID))
        .collect();
    assert_eq!(
        all_alpha.len() - 2,
        6,
        "every relation carries the six system attributes"
    );
    let alpha_attrs: Vec<_> = all_alpha
        .into_iter()
        .filter(|r| matches!(r[anum], Value::Int2(n) if n > 0))
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
        (
            "regprocedure",
            PgType::Reg(crabgresql_types::RegKind::Procedure),
        ),
        ("regoper", PgType::Reg(crabgresql_types::RegKind::Oper)),
        (
            "regoperator",
            PgType::Reg(crabgresql_types::RegKind::Operator),
        ),
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

/// Nothing in this build can be a domain — there is no `CREATE DOMAIN` — so
/// every row carries the BKI defaults.
///
/// Position matters as much as presence: a client issuing `SELECT *` reads the
/// columns by ordinal, and DataGrip's type list reads `typtypmod` through
/// `format_type(typbasetype, typtypmod)` — a `-1` in the wrong slot renders the
/// wrong definition rather than failing. The ordinals below are `pg_attribute`'s
/// on a live PostgreSQL 18.4 (`attnum` 25–32).
#[test]
fn pg_type_carries_the_domain_columns_where_postgres_does() {
    let schema = catalogs::types::pg_type_schema();
    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        &names[24..],
        &[
            "typnotnull",
            "typbasetype",
            "typtypmod",
            "typndims",
            "typcollation",
            "typdefaultbin",
            "typdefault",
            "typacl",
        ]
    );

    // Both row builders, since each spells the columns out separately.
    let enum_rows = catalogs::types::pg_type_user_rows(&[CatalogUserType {
        oid: 20000,
        name: "mood".to_string(),
        enum_labels: Some(vec!["ok".to_string()]),
        domain: None,
    }]);
    let int4 = catalogs::types::pg_type_builtin_rows()
        .into_iter()
        .find(|r| type_col(r, &schema, "typname") == Value::Text("int4".to_string()))
        .expect("int4 row present");
    for row in [&int4, &enum_rows[0]] {
        assert_eq!(type_col(row, &schema, "typnotnull"), Value::Bool(false));
        assert_eq!(type_col(row, &schema, "typbasetype"), Value::Oid(0));
        assert_eq!(type_col(row, &schema, "typtypmod"), Value::Int4(-1));
        assert_eq!(type_col(row, &schema, "typndims"), Value::Int4(0));
        assert_eq!(type_col(row, &schema, "typdefaultbin"), Value::Null);
        assert_eq!(type_col(row, &schema, "typdefault"), Value::Null);
    }
    // typcollation sits in the middle of the six and still comes from the row
    // itself, so a collatable type keeps reporting its collation.
    let text = catalogs::types::pg_type_builtin_rows()
        .into_iter()
        .find(|r| type_col(r, &schema, "typname") == Value::Text("text".to_string()))
        .expect("text row present");
    assert_eq!(type_col(&text, &schema, "typcollation"), Value::Oid(100));
    assert_eq!(type_col(&int4, &schema, "typcollation"), Value::Oid(0));
    assert_eq!(
        type_col(&enum_rows[0], &schema, "typcollation"),
        Value::Oid(0)
    );
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
            domain: None,
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

/// `pg_conversion` describes upstream's encoding conversions. This server
/// speaks UTF-8 and nothing else, so none of them can ever run — the widest
/// deviation of the wave, and the reason it says so in its module docs.
///
/// The encoding *numbers* are the risk here: unlike everything else in these
/// catalogs they are not in the vendored data. The generated rows carry the
/// encoding's *name*, and [`crabgresql_types::encoding`] — the one table this
/// build records the numbering in — turns it into an integer, so a name that
/// answers to no encoding arrives as `pg_char_to_encoding`'s own miss
/// sentinel, `-1`. Refusing that here is what replaces the build-time check
/// codegen used to do against a second copy of the table.
///
/// A *wrong* number would still be silent — a conversion between two plausible
/// encodings — so the second check is against the one number a client can name
/// for itself, `UTF8`.
#[test]
fn pg_conversion_numbers_the_encodings_it_converts_between() -> anyhow::Result<()> {
    let cat = SystemCatalog::new();
    let build = |name: &str| required(cat.build_pg_catalog(name), name);
    let (schema, rows) = build("pg_conversion")?;
    let (proc_schema, proc_rows) = build("pg_proc")?;
    let at = |schema: &TableSchema, name: &str| schema.column_index(name).expect("column exists");
    let procs: std::collections::HashSet<u32> = proc_rows
        .iter()
        .map(|r| match r[at(&proc_schema, "oid")] {
            Value::Oid(oid) => oid,
            ref other => panic!("expected an OID, got {other:?}"),
        })
        .collect();
    let int_at = |row: &[Value], i: usize| match row[i] {
        Value::Int4(n) => n,
        ref other => panic!("expected an int4, got {other:?}"),
    };

    assert_eq!(rows.len(), PG_CONVERSION_ROWS.len());
    for row in &rows {
        let Value::Reg(ref proc) = row[at(&schema, "conproc")] else {
            anyhow::bail!("conproc is not a regproc");
        };
        assert!(procs.contains(&proc.oid), "conproc {} dangles", proc.oid);
        let (from, to) = (
            int_at(row, at(&schema, "conforencoding")),
            int_at(row, at(&schema, "contoencoding")),
        );
        assert_ne!(from, to, "a conversion from an encoding to itself");
        for n in [from, to] {
            // `-1` is what a name no encoding answers to resolves to, and an
            // out-of-range number renders as the empty string; both mean the
            // row names an encoding this build cannot number.
            assert_ne!(n, -1, "an encoding this build cannot name");
            assert!(
                !crabgresql_types::encoding::encoding_to_char(n).is_empty(),
                "encoding number {n} names nothing"
            );
        }
        assert_eq!(row[at(&schema, "condefault")], Value::Bool(true));
    }

    // `UTF8` is 6 — the one encoding number this build can check against
    // something other than the transcribed table, because it is the encoding
    // every connection here is in. A shifted table would put a different
    // encoding on both sides of every `*_to_utf8` conversion.
    const UTF8: i32 = 6;
    let name_at = |row: &[Value]| match row[at(&schema, "conname")] {
        Value::Text(ref name) => name.clone(),
        ref other => panic!("expected a name, got {other:?}"),
    };
    let mut checked = 0;
    for row in &rows {
        let name = name_at(row);
        if let Some(source) = name.strip_suffix("_to_utf8") {
            assert_eq!(
                int_at(row, at(&schema, "contoencoding")),
                UTF8,
                "{name} does not convert to UTF8"
            );
            assert_ne!(int_at(row, at(&schema, "conforencoding")), UTF8);
            assert!(!source.is_empty());
            checked += 1;
        } else if name.starts_with("utf8_to_") {
            assert_eq!(
                int_at(row, at(&schema, "conforencoding")),
                UTF8,
                "{name} does not convert from UTF8"
            );
            checked += 1;
        }
    }
    // The conversions to and from UTF8 are the bulk of the relation; a table
    // that matched none of them would pass every assertion above.
    assert!(checked > 40, "only {checked} conversions name UTF8");
    Ok(())
}

/// The five `pg_ts_*` relations, half generated from the `.dat` and half
/// reconstructed from the snowball language list.
///
/// The reconstruction is where the risk is: the `.dat` half is checked by the
/// same machinery every other generated catalog is, but the twenty-nine
/// snowball configurations are built from a list transcribed here, so what
/// keeps them honest is that they have to *join* like the generated half —
/// every dictionary at the snowball template, every configuration at the
/// default parser, and every configuration mapping the same nineteen token
/// types the `simple` one does.
///
/// Nothing pins a snowball OID: those come from `initdb`'s counter rather than
/// from vendored data. See `catalogs::textsearch`.
#[test]
fn pg_ts_relations_publish_the_bootstrap_and_snowball_halves() -> anyhow::Result<()> {
    let cat = SystemCatalog::new();
    let build = |name: &str| required(cat.build_pg_catalog(name), name);
    let (parser_schema, parser_rows) = build("pg_ts_parser")?;
    let (template_schema, template_rows) = build("pg_ts_template")?;
    let (dict_schema, dict_rows) = build("pg_ts_dict")?;
    let (config_schema, config_rows) = build("pg_ts_config")?;
    let (map_schema, map_rows) = build("pg_ts_config_map")?;
    let (proc_schema, proc_rows) = build("pg_proc")?;
    let at = |schema: &TableSchema, name: &str| schema.column_index(name).expect("column exists");
    let oid_at = |row: &[Value], i: usize| match row[i] {
        Value::Oid(oid) => oid,
        ref other => panic!("expected an OID, got {other:?}"),
    };
    let name_at = |row: &[Value], i: usize| match row[i] {
        Value::Text(ref name) => name.clone(),
        ref other => panic!("expected a name, got {other:?}"),
    };

    // The counts a stock PostgreSQL 18.4 reports, which is the whole point of
    // reconstructing the snowball half rather than serving the `.dat` alone.
    assert_eq!(parser_rows.len(), 1);
    assert_eq!(template_rows.len(), 5);
    assert_eq!(dict_rows.len(), 30);
    assert_eq!(config_rows.len(), 30);
    assert_eq!(map_rows.len(), 570);

    // The snowball pair among them is the one `pg_proc` half no `.dat`
    // defines and `catalogs::proc` spells out.
    let procs: std::collections::HashSet<u32> = proc_rows
        .iter()
        .map(|r| oid_at(r, at(&proc_schema, "oid")))
        .collect();
    let reg_at = |row: &[Value], i: usize| match row[i] {
        Value::Reg(ref reg) => reg.oid,
        ref other => panic!("expected a regproc, got {other:?}"),
    };
    // None of the seven may be zero: a parser with no tokenizer or a template
    // with no lexizer is not one, and upstream's data never leaves them out —
    // so a zero here would mean a reference that failed to resolve rather than
    // an absent function.
    for column in [
        "prsstart",
        "prstoken",
        "prsend",
        "prsheadline",
        "prslextype",
    ] {
        let oid = reg_at(&parser_rows[0], at(&parser_schema, column));
        assert_ne!(oid, 0, "the default parser has no {column}");
        assert!(procs.contains(&oid), "{column} {oid} dangles");
    }
    for row in &template_rows {
        for column in ["tmplinit", "tmpllexize"] {
            let oid = reg_at(row, at(&template_schema, column));
            assert_ne!(oid, 0, "a template has no {column}");
            assert!(procs.contains(&oid), "{column} {oid} dangles");
        }
    }

    let template_oids: std::collections::HashMap<u32, String> = template_rows
        .iter()
        .map(|r| {
            (
                oid_at(r, at(&template_schema, "oid")),
                name_at(r, at(&template_schema, "tmplname")),
            )
        })
        .collect();
    let dicts: std::collections::HashMap<u32, String> = dict_rows
        .iter()
        .map(|r| {
            (
                oid_at(r, at(&dict_schema, "oid")),
                name_at(r, at(&dict_schema, "dictname")),
            )
        })
        .collect();
    let configs: std::collections::HashMap<u32, String> = config_rows
        .iter()
        .map(|r| {
            (
                oid_at(r, at(&config_schema, "oid")),
                name_at(r, at(&config_schema, "cfgname")),
            )
        })
        .collect();
    // Names are unique per relation: an OID collision between the generated
    // band and the reconstructed one would show up here first.
    assert_eq!(dicts.len(), dict_rows.len());
    assert_eq!(configs.len(), config_rows.len());
    // An OID is only required to be unique within its own catalog, but
    // `initdb` hands these out from one counter, so no dictionary and
    // configuration share a number. Reconstructing the pair with the same
    // formula for both — the off-by-one that is easiest to write — would
    // satisfy every other check here.
    for (oid, name) in &dicts {
        assert!(
            !configs.contains_key(oid),
            "{name} and configuration {} share an OID",
            configs[oid]
        );
    }
    for (oid, name) in &template_oids {
        assert!(
            !dicts.contains_key(oid) && !configs.contains_key(oid),
            "template {name} shares an OID with a dictionary or configuration"
        );
    }

    let parser_oid = oid_at(&parser_rows[0], at(&parser_schema, "oid"));
    for row in &dict_rows {
        let name = name_at(row, at(&dict_schema, "dictname"));
        let template = oid_at(row, at(&dict_schema, "dicttemplate"));
        let template = template_oids
            .get(&template)
            .unwrap_or_else(|| panic!("{name}'s template dangles"));
        if name == "simple" {
            assert_eq!(template, "simple");
            assert_eq!(row[at(&dict_schema, "dictinitoption")], Value::Null);
        } else {
            assert_eq!(template, "snowball", "{name} is not a snowball dictionary");
            let language = name.strip_suffix("_stem").expect("named <language>_stem");
            let Value::Text(ref option) = row[at(&dict_schema, "dictinitoption")] else {
                anyhow::bail!("{name} has no init option");
            };
            assert!(
                option.starts_with(&format!("language = '{language}'")),
                "{name} configures {option:?}"
            );
        }
    }
    for row in &config_rows {
        assert_eq!(oid_at(row, at(&config_schema, "cfgparser")), parser_oid);
    }

    // The token set is not written out here either: it is whatever the
    // `simple` configuration maps, which is where the `.dat` states it.
    let simple_config = config_rows
        .iter()
        .find(|r| name_at(r, at(&config_schema, "cfgname")) == "simple")
        .map(|r| oid_at(r, at(&config_schema, "oid")))
        .expect("the simple configuration");
    let tokens_of = |cfg: u32| -> Vec<i32> {
        let mut tokens: Vec<i32> = map_rows
            .iter()
            .filter(|r| oid_at(r, at(&map_schema, "mapcfg")) == cfg)
            .map(|r| match r[at(&map_schema, "maptokentype")] {
                Value::Int4(token) => token,
                ref other => panic!("expected an int4, got {other:?}"),
            })
            .collect();
        tokens.sort_unstable();
        tokens
    };
    let expected_tokens = tokens_of(simple_config);
    assert_eq!(expected_tokens.len(), 19);
    for (&cfg, name) in &configs {
        assert_eq!(tokens_of(cfg), expected_tokens, "the token map of {name}");
    }
    for row in &map_rows {
        assert_eq!(row[at(&map_schema, "mapseqno")], Value::Int4(1));
        let dict = oid_at(row, at(&map_schema, "mapdict"));
        assert!(dicts.contains_key(&dict), "mapdict {dict} dangles");
        let cfg = oid_at(row, at(&map_schema, "mapcfg"));
        assert!(configs.contains_key(&cfg), "mapcfg {cfg} dangles");
    }

    // The split `snowball_create.sql` makes: a language configuration sends
    // the six word-shaped token types to its own stemmer and the other
    // thirteen to `simple`. Checked on `english`, the one anybody uses.
    let english = *configs
        .iter()
        .find(|(_, name)| *name == "english")
        .map(|(oid, _)| oid)
        .expect("the english configuration");
    let mut to_stemmer = 0;
    let mut to_simple = 0;
    for row in map_rows
        .iter()
        .filter(|r| oid_at(r, at(&map_schema, "mapcfg")) == english)
    {
        match dicts[&oid_at(row, at(&map_schema, "mapdict"))].as_str() {
            "english_stem" => to_stemmer += 1,
            "simple" => to_simple += 1,
            other => panic!("english maps a token to {other}"),
        }
    }
    assert_eq!((to_stemmer, to_simple), (6, 13));
    Ok(())
}

/// `pg_aggregate` describes upstream's aggregates: the transition and final
/// functions each one is built from, and the ordering operator `min`/`max` are
/// equivalent to.
///
/// # The direction this cannot check
///
/// The same one `pg_operator` has. The executor's accumulators are Rust, not
/// these functions, so a row is a description rather than a plan. What is
/// checkable is that every reference lands, that the key names a function
/// `pg_proc` agrees is an aggregate, and that the six aggregates this server
/// evaluates are all described.
#[test]
fn pg_aggregate_describes_upstreams_aggregates() -> anyhow::Result<()> {
    let cat = SystemCatalog::new();
    let build = |name: &str| required(cat.build_pg_catalog(name), name);
    let (schema, rows) = build("pg_aggregate")?;
    let (proc_schema, proc_rows) = build("pg_proc")?;
    let (type_schema, type_rows) = build("pg_type")?;
    let (operator_schema, operator_rows) = build("pg_operator")?;
    let at = |schema: &TableSchema, name: &str| schema.column_index(name).expect("column exists");
    let oid_at = |row: &[Value], i: usize| match row[i] {
        Value::Oid(oid) => oid,
        ref other => panic!("expected an OID, got {other:?}"),
    };
    let types: std::collections::HashSet<u32> = type_rows
        .iter()
        .map(|r| oid_at(r, at(&type_schema, "oid")))
        .collect();
    let operators: std::collections::HashSet<u32> = operator_rows
        .iter()
        .map(|r| oid_at(r, at(&operator_schema, "oid")))
        .collect();
    let procs: std::collections::HashMap<u32, (String, String)> = proc_rows
        .iter()
        .map(|r| {
            let name = match r[at(&proc_schema, "proname")] {
                Value::Text(ref name) => name.clone(),
                ref other => panic!("expected a name, got {other:?}"),
            };
            let kind = match r[at(&proc_schema, "prokind")] {
                Value::Char(c) => (c as char).to_string(),
                ref other => panic!("expected a \"char\", got {other:?}"),
            };
            (oid_at(r, at(&proc_schema, "oid")), (name, kind))
        })
        .collect();

    assert_eq!(rows.len(), PG_AGGREGATE_ROWS.len());
    let reg_at = |row: &[Value], i: usize| match row[i] {
        Value::Reg(ref reg) => reg.oid,
        ref other => panic!("expected a regproc, got {other:?}"),
    };
    let mut names = std::collections::BTreeSet::new();
    for row in &rows {
        // The key is a function this build publishes, and that function says
        // it is an aggregate. A `pg_aggregate` row over a plain function would
        // be a claim `pg_proc` contradicts.
        let key = reg_at(row, at(&schema, "aggfnoid"));
        let (name, kind) = procs
            .get(&key)
            .unwrap_or_else(|| panic!("aggfnoid {key} names no pg_proc row"));
        assert_eq!(kind, "a", "{name} is an aggregate here but not in pg_proc");
        names.insert(name.clone());

        // An aggregate with nothing to fold rows with is not one; the other
        // eight support functions genuinely may be absent.
        for (column, required) in [
            ("aggtransfn", true),
            ("aggfinalfn", false),
            ("aggcombinefn", false),
            ("aggserialfn", false),
            ("aggdeserialfn", false),
            ("aggmtransfn", false),
            ("aggminvtransfn", false),
            ("aggmfinalfn", false),
        ] {
            let oid = reg_at(row, at(&schema, column));
            assert!(
                (!required && oid == 0) || procs.contains_key(&oid),
                "{column} {oid} of {name} dangles"
            );
        }
        let transtype = oid_at(row, at(&schema, "aggtranstype"));
        assert!(types.contains(&transtype), "aggtranstype of {name} dangles");
        let mtranstype = oid_at(row, at(&schema, "aggmtranstype"));
        assert!(
            mtranstype == 0 || types.contains(&mtranstype),
            "aggmtranstype of {name} dangles"
        );
        let sortop = oid_at(row, at(&schema, "aggsortop"));
        assert!(
            sortop == 0 || operators.contains(&sortop),
            "aggsortop of {name} dangles"
        );
        // A moving-window aggregate names its transition function and its
        // inverse together: one without the other is a frame that can add rows
        // and never remove them.
        assert_eq!(
            reg_at(row, at(&schema, "aggmtransfn")) == 0,
            reg_at(row, at(&schema, "aggminvtransfn")) == 0,
            "{name} has half a moving-window aggregate"
        );
    }

    // The aggregates this server evaluates — `crabgresql-binder`'s `lookup_agg`,
    // which is a Rust match rather than a table, so the list is repeated here
    // rather than read. That is also why the converse is not asserted: an
    // aggregate added there and missing here would not fail this test, and the
    // smoke suite is where the two are exercised together.
    for aggregate in [
        "count",
        "min",
        "max",
        "sum",
        "avg",
        "string_agg",
        "array_agg",
    ] {
        assert!(names.contains(aggregate), "{aggregate} is not described");
    }

    // `max(int4)` in full: the shape a client reads out of \da, and the one
    // row where every optional column is populated the interesting way.
    let max_int4 = rows
        .iter()
        .find(|r| {
            procs[&reg_at(r, at(&schema, "aggfnoid"))].0 == "max"
                && oid_at(r, at(&schema, "aggtranstype")) == 23
        })
        .expect("max(int4) is described");
    assert_eq!(
        procs[&reg_at(max_int4, at(&schema, "aggtransfn"))].0,
        "int4larger"
    );
    assert_eq!(max_int4[at(&schema, "aggkind")], crate::cols::chr('n'));
    // Its sort operator is `>`: MAX is the last row of an ascending order.
    let sortop = oid_at(max_int4, at(&schema, "aggsortop"));
    assert_eq!(
        operator_rows
            .iter()
            .find(|r| oid_at(r, at(&operator_schema, "oid")) == sortop)
            .map(|r| r[at(&operator_schema, "oprname")].clone()),
        Some(Value::Text(">".to_string()))
    );
    Ok(())
}

/// `pg_operator` describes upstream's operator set. Every reference out of it
/// has to land somewhere — including the two that point back into itself, a
/// commutator and a negator, which is the reference cycle codegen's two phases
/// exist for.
///
/// # The direction this cannot check
///
/// Operators are resolved by `crabgresql-binder`'s own code, not by reading
/// this table, and that resolver has no enumerable registry — so nothing here
/// can measure how much of the 805 rows this server actually evaluates. What
/// it can do is pin, for a handful of operators the server demonstrably runs,
/// that the row describing each one says the right thing. The converse — that
/// some described operator is *not* implemented — is stated in
/// `catalogs::operator`'s docs and shown end to end by the smoke suite.
#[test]
fn pg_operator_describes_upstreams_operators() -> anyhow::Result<()> {
    let cat = SystemCatalog::new();
    let build = |name: &str| required(cat.build_pg_catalog(name), name);
    let (schema, rows) = build("pg_operator")?;
    let (type_schema, type_rows) = build("pg_type")?;
    let (proc_schema, proc_rows) = build("pg_proc")?;
    let at = |schema: &TableSchema, name: &str| schema.column_index(name).expect("column exists");
    let oid_at = |row: &[Value], i: usize| match row[i] {
        Value::Oid(oid) => oid,
        ref other => panic!("expected an OID, got {other:?}"),
    };
    let oids = |rows: &[Vec<Value>], schema: &TableSchema| -> std::collections::HashSet<u32> {
        let i = at(schema, "oid");
        rows.iter().map(|r| oid_at(r, i)).collect()
    };
    let types = oids(&type_rows, &type_schema);
    let procs = oids(&proc_rows, &proc_schema);
    let operators = oids(&rows, &schema);

    for row in &rows {
        // A prefix operator has no left operand and writes 0 there; nothing
        // has no right operand, since PostgreSQL 14 dropped postfix operators.
        let (kind, left) = (
            row[at(&schema, "oprkind")].clone(),
            oid_at(row, at(&schema, "oprleft")),
        );
        assert_eq!(
            left == 0,
            kind == crate::cols::chr('l'),
            "oprleft and oprkind disagree"
        );
        assert!(left == 0 || types.contains(&left), "oprleft {left} dangles");
        for column in ["oprright", "oprresult"] {
            let oid = oid_at(row, at(&schema, column));
            assert!(types.contains(&oid), "{column} {oid} dangles");
        }
        for column in ["oprcom", "oprnegate"] {
            let oid = oid_at(row, at(&schema, column));
            assert!(
                oid == 0 || operators.contains(&oid),
                "{column} {oid} dangles"
            );
        }
        // An operator nothing evaluates is not one; the two selectivity
        // estimators are the pair upstream really does leave out.
        for (column, required) in [("oprcode", true), ("oprrest", false), ("oprjoin", false)] {
            let Value::Reg(ref proc) = row[at(&schema, column)] else {
                anyhow::bail!("{column} is not a regproc");
            };
            // A zero is only ever the catalog's "no function", which renders
            // as `-`. Permitting a zero on its own would also permit a column
            // that names a real function and points at nothing — which is
            // exactly what an unresolved reference used to produce.
            assert_eq!(
                proc.oid == 0,
                proc.name == "-",
                "{column} reports oid {} as {:?}",
                proc.oid,
                proc.name
            );
            assert!(
                (!required && proc.oid == 0) || procs.contains(&proc.oid),
                "{column} {} dangles",
                proc.oid
            );
        }
    }

    // A commutator commutes: if A says B, B says A, and their operands are
    // swapped. This is the property a wrong resolution would break while every
    // universe check above still passed.
    let by_oid: std::collections::HashMap<u32, &Vec<Value>> = rows
        .iter()
        .map(|r| (oid_at(r, at(&schema, "oid")), r))
        .collect();
    for row in &rows {
        let com = oid_at(row, at(&schema, "oprcom"));
        if com == 0 {
            continue;
        }
        let other = by_oid[&com];
        assert_eq!(
            oid_at(other, at(&schema, "oprcom")),
            oid_at(row, at(&schema, "oid")),
            "a commutator that does not commute back"
        );
        assert_eq!(
            oid_at(other, at(&schema, "oprleft")),
            oid_at(row, at(&schema, "oprright"))
        );
        assert_eq!(
            oid_at(other, at(&schema, "oprright")),
            oid_at(row, at(&schema, "oprleft"))
        );
    }

    // Operators this server evaluates, and what their rows say. Named, not
    // OID-pinned: `pg_operator.dat` assigns these OIDs itself, but the name is
    // what a client reads.
    const INT4: u32 = 23;
    const TEXT: u32 = 25;
    const BOOL: u32 = 16;
    let described = |name: &str, left: u32, right: u32| {
        rows.iter()
            .find(|r| {
                r[at(&schema, "oprname")] == Value::Text(name.to_string())
                    && oid_at(r, at(&schema, "oprleft")) == left
                    && oid_at(r, at(&schema, "oprright")) == right
            })
            .map(|r| {
                let Value::Reg(ref code) = r[at(&schema, "oprcode")] else {
                    panic!("oprcode is not a regproc");
                };
                (oid_at(r, at(&schema, "oprresult")), code.name.clone())
            })
    };
    for (name, left, right, result, code) in [
        ("=", INT4, INT4, BOOL, "int4eq"),
        ("<>", INT4, INT4, BOOL, "int4ne"),
        ("<", INT4, INT4, BOOL, "int4lt"),
        ("+", INT4, INT4, INT4, "int4pl"),
        ("*", INT4, INT4, INT4, "int4mul"),
        ("||", TEXT, TEXT, TEXT, "textcat"),
        ("~~", TEXT, TEXT, BOOL, "textlike"),
        ("=", TEXT, TEXT, BOOL, "texteq"),
    ] {
        assert_eq!(
            described(name, left, right),
            Some((result, code.to_string())),
            "the row for {name}({left},{right})"
        );
    }
    assert_eq!(described("-", 0, INT4), Some((INT4, "int4um".to_string())));
    Ok(())
}

/// The `regoper` lookups read the same rows. Probed against PostgreSQL 18.4:
/// `'||/'::regoper` is 597, while `'+'::regoper` raises "more than one operator
/// named +" because every operand pair shares the name.
#[test]
fn operator_lookups_report_how_many_share_a_name() {
    assert_eq!(builtin_oper_oids("||/"), vec![597]);
    assert_eq!(builtin_oper_row(597).map(|row| row.oprname), Some("||/"));
    assert!(
        builtin_oper_oids("+").len() > 1,
        "`+` names one operator per operand pair"
    );
    assert!(builtin_oper_oids("+").contains(&551), "int4pl is `+`");
    assert_eq!(builtin_oper_oids("nosuchoperator"), Vec::<u32>::new());
    assert!(builtin_oper_row(0).is_none());
    assert!(builtin_oper_row(999_999).is_none());

    // Built-ins live in `pg_catalog`, so any other qualifier names nothing.
    let cat = SystemCatalog::new();
    assert_eq!(cat.oper_oids(Some("pg_catalog"), "||/"), vec![597]);
    assert_eq!(cat.oper_oids(Some("public"), "||/"), Vec::<u32>::new());
    assert_eq!(
        cat.oper_signature(597),
        Some((
            "pg_catalog".to_string(),
            "||/".to_string(),
            0,
            crabgresql_types::oid::FLOAT8
        ))
    );
}

/// What the *operand types* buy `regoperator`: the name `+` that
/// [`operator_lookups_report_how_many_share_a_name`] cannot resolve becomes one
/// operator once they are given. Probed against PostgreSQL 18.4:
/// `'+(int4,int4)'::regoperator` is 551 and `484::regoperator` is
/// `-(NONE,bigint)` — a prefix operator, whose absent left operand the catalog
/// stores as 0.
#[test]
fn an_operator_is_unique_once_its_operands_are_given() {
    use crabgresql_types::oid::{INT4, INT8};
    assert_eq!(builtin_oper_oid("+", INT4, INT4), Some(551));
    assert_ne!(builtin_oper_oid("+", INT8, INT8), Some(551));
    assert_eq!(builtin_oper_oid("+", INT4, 0), None);
    assert_eq!(builtin_oper_oid("nosuchoperator", INT4, INT4), None);
    assert_eq!(builtin_oper_oid("-", 0, INT8), Some(484));
    assert_eq!(
        builtin_oper_row(484).map(|row| (row.oprname, row.oprleft, row.oprright)),
        Some(("-", 0, INT8))
    );

    let cat = SystemCatalog::new();
    assert_eq!(cat.oper_oid(Some("pg_catalog"), "+", INT4, INT4), Some(551));
    assert_eq!(cat.oper_oid(None, "+", INT4, INT4), Some(551));
    assert_eq!(cat.oper_oid(Some("public"), "+", INT4, INT4), None);
}

/// `regprocedure` names a function by its whole signature, which is what makes
/// an overloaded name resolvable at all: `int4pl` and `int8pl` share the `+`
/// operator's job under different argument types, and `regproc` can only take a
/// name no other function carries.
#[test]
fn function_lookups_resolve_a_whole_signature() {
    let int4 = crabgresql_types::oid::INT4;
    let oid = builtin_proc_oid_by_args("int4pl", &[int4, int4])
        .expect("int4pl is `+`'s oprcode, so pg_proc publishes it");
    assert_eq!(
        builtin_proc_signature(oid),
        Some(("int4pl", &[int4, int4][..]))
    );
    // The name alone is not the key: a signature no function has is a miss,
    // not a different overload.
    assert_eq!(builtin_proc_oid_by_args("int4pl", &[int4]), None);
    assert_eq!(builtin_proc_oid_by_args("nosuchfunc", &[]), None);
    assert_eq!(builtin_proc_signature(0), None);

    // Built-ins live in `pg_catalog`, so any other qualifier names nothing —
    // and the namespace comes back with the signature, as `regprocedure`
    // output needs it.
    let cat = SystemCatalog::new();
    assert_eq!(
        cat.proc_oid_by_signature(Some("pg_catalog"), "int4pl", &[int4, int4]),
        Some(oid)
    );
    assert_eq!(
        cat.proc_oid_by_signature(Some("public"), "int4pl", &[int4, int4]),
        None
    );
    assert_eq!(
        cat.proc_signature(oid),
        Some((
            "pg_catalog".to_string(),
            "int4pl".to_string(),
            vec![int4, int4]
        ))
    );
}

/// `pg_amop` and `pg_amproc` finish the operator-class stack: every reference
/// out of them has to land on a row that exists, or a client joining them —
/// which is all `\dAo` and `\dAp` do — reads a dangling OID.
///
/// The OIDs of these two catalogs are generated by upstream's counter rather
/// than written in the data, and the count drifts between major versions (18.4
/// ships 945 `pg_amop` rows against 19devel's 951), so nothing here pins a raw
/// OID. What it pins is the shape a join sees.
#[test]
fn pg_amop_and_pg_amproc_join_to_what_they_name() -> anyhow::Result<()> {
    let cat = SystemCatalog::new();
    let build = |name: &str| required(cat.build_pg_catalog(name), name);
    let (amop_schema, amop_rows) = build("pg_amop")?;
    let (amproc_schema, amproc_rows) = build("pg_amproc")?;
    let (fam_schema, fam_rows) = build("pg_opfamily")?;
    let (am_schema, am_rows) = build("pg_am")?;
    let (type_schema, type_rows) = build("pg_type")?;
    let (proc_schema, proc_rows) = build("pg_proc")?;

    let at = |schema: &TableSchema, name: &str| schema.column_index(name).expect("column exists");
    let oids = |rows: &[Vec<Value>], schema: &TableSchema| -> std::collections::HashSet<u32> {
        let i = at(schema, "oid");
        rows.iter()
            .filter_map(|r| match r[i] {
                Value::Oid(oid) => Some(oid),
                _ => None,
            })
            .collect()
    };
    let (operator_schema, operator_rows) = build("pg_operator")?;
    let operators = oids(&operator_rows, &operator_schema);
    let families = oids(&fam_rows, &fam_schema);
    let methods = oids(&am_rows, &am_schema);
    let types = oids(&type_rows, &type_schema);
    let procs = oids(&proc_rows, &proc_schema);
    let oid_at = |row: &[Value], i: usize| match row[i] {
        Value::Oid(oid) => oid,
        ref other => panic!("expected an OID, got {other:?}"),
    };

    assert_eq!(amop_rows.len(), PG_AMOP_ROWS.len());
    for row in &amop_rows {
        for (column, universe) in [
            ("amopfamily", &families),
            ("amoplefttype", &types),
            ("amoprighttype", &types),
            ("amopmethod", &methods),
        ] {
            let oid = oid_at(row, at(&amop_schema, column));
            assert!(universe.contains(&oid), "{column} {oid} dangles");
        }
        // Only an ordering operator names a sort family; a search one leaves
        // the column 0, which is PostgreSQL's "no such object" and not a
        // reference at all.
        let sortfamily = oid_at(row, at(&amop_schema, "amopsortfamily"));
        assert!(
            sortfamily == 0 || families.contains(&sortfamily),
            "amopsortfamily {sortfamily} dangles"
        );
        let opr = oid_at(row, at(&amop_schema, "amopopr"));
        assert!(operators.contains(&opr), "amopopr {opr} dangles");
    }

    assert_eq!(amproc_rows.len(), PG_AMPROC_ROWS.len());
    let amproc_family = at(&amproc_schema, "amprocfamily");
    let amproc_col = at(&amproc_schema, "amproc");
    for row in &amproc_rows {
        for (column, universe) in [
            ("amprocfamily", &families),
            ("amproclefttype", &types),
            ("amprocrighttype", &types),
        ] {
            let oid = oid_at(row, at(&amproc_schema, column));
            assert!(universe.contains(&oid), "{column} {oid} dangles");
        }
        let Value::Reg(ref proc) = row[amproc_col] else {
            anyhow::bail!("amproc is not a regproc");
        };
        assert!(procs.contains(&proc.oid), "amproc {} dangles", proc.oid);
    }

    // The point of the pair: a type whose default btree class this build
    // already reported now also says which function does the comparing. A
    // class with no support function 1 is a class btree could not use.
    let name_at = |row: &[Value], i: usize| match row[i] {
        Value::Text(ref name) => name.clone(),
        ref other => panic!("expected a name, got {other:?}"),
    };
    let (opclass_schema, opclass_rows) = build("pg_opclass")?;
    let (opc_family, opc_intype, opc_method, opc_default, opc_name) = (
        at(&opclass_schema, "opcfamily"),
        at(&opclass_schema, "opcintype"),
        at(&opclass_schema, "opcmethod"),
        at(&opclass_schema, "opcdefault"),
        at(&opclass_schema, "opcname"),
    );
    const BTREE: u32 = 403;
    for row in &opclass_rows {
        if oid_at(row, opc_method) != BTREE || row[opc_default] != Value::Bool(true) {
            continue;
        }
        let (family, intype) = (oid_at(row, opc_family), oid_at(row, opc_intype));
        assert!(
            amproc_rows.iter().any(|p| {
                oid_at(p, amproc_family) == family
                    && oid_at(p, at(&amproc_schema, "amproclefttype")) == intype
                    && p[at(&amproc_schema, "amprocnum")] == Value::Int2(1)
            }),
            "btree class {} has no comparison support function",
            name_at(row, opc_name)
        );
    }

    // A universe check cannot tell `int4` from `int2` — both are types that
    // exist — so pin what the rows actually say for one family per method.
    // By name, not by OID: these two catalogs are numbered by upstream's
    // counter and the numbers move between majors.
    const INT4: u32 = 23;
    let support = |family: u32, num: i16| {
        amproc_rows
            .iter()
            .find(|p| {
                oid_at(p, amproc_family) == family
                    && oid_at(p, at(&amproc_schema, "amproclefttype")) == INT4
                    && oid_at(p, at(&amproc_schema, "amprocrighttype")) == INT4
                    && p[at(&amproc_schema, "amprocnum")] == Value::Int2(num)
            })
            .map(|p| match p[amproc_col] {
                Value::Reg(ref proc) => proc.name.clone(),
                ref other => panic!("expected a regproc, got {other:?}"),
            })
    };
    const BTREE_INTEGER_OPS: u32 = 1976;
    const HASH_INTEGER_OPS: u32 = 1977;
    assert_eq!(support(BTREE_INTEGER_OPS, 1).as_deref(), Some("btint4cmp"));
    assert_eq!(support(HASH_INTEGER_OPS, 1).as_deref(), Some("hashint4"));

    // The join that names each strategy's operator is in the smoke suite.
    let strategies: Vec<i16> = (1..=5)
        .filter(|n| {
            amop_rows.iter().any(|r| {
                oid_at(r, at(&amop_schema, "amopfamily")) == BTREE_INTEGER_OPS
                    && oid_at(r, at(&amop_schema, "amoplefttype")) == INT4
                    && oid_at(r, at(&amop_schema, "amoprighttype")) == INT4
                    && r[at(&amop_schema, "amopstrategy")] == Value::Int2(*n)
            })
        })
        .collect();
    assert_eq!(strategies, vec![1, 2, 3, 4, 5]);
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

/// `pg_class.relhassubclass` is true for exactly the relations some other one
/// names as its parent — through `PARTITION OF` *or* through `INHERITS (...)`.
///
/// Walking only one of the two edges would leave half the hierarchies looking
/// childless while `pg_inherits` still published their edges, and a client
/// reads this flag to decide whether to look for children at all.
#[test]
fn pg_class_relhassubclass_marks_both_kinds_of_parent() -> anyhow::Result<()> {
    use crabgresql_storage_api::{
        Column, InheritParent, PartitionBound, PartitionBoundDatum, PartitionOf, PartitionScheme,
        PartitionStrategy, TableSchema,
    };
    use crabgresql_types::PgType;

    let plain = |name: &str| TableSchema::new(name, vec![Column::new("a", PgType::Int4)]);

    let mut part = plain("part");
    part.partition_scheme = Some(PartitionScheme {
        strategy: PartitionStrategy::Range,
        key_columns: vec![0],
    });
    let mut leaf = plain("part_1");
    leaf.partition_of = Some(PartitionOf {
        parent_namespace: "public".to_string(),
        parent_name: "part".to_string(),
        key_columns: vec![0],
        bound: PartitionBound {
            from: vec![PartitionBoundDatum::Value(Value::Int4(0))],
            to: vec![PartitionBoundDatum::Value(Value::Int4(10))],
        },
    });
    let mut child = plain("child");
    child.inherits = vec![InheritParent {
        namespace: "public".to_string(),
        name: "base".to_string(),
    }];

    let cat = SystemCatalog::with_catalog_relations("db", "owner", {
        vec![
            CatalogRelation::permanent(plain("lonely")),
            CatalogRelation::permanent(plain("base")),
            CatalogRelation::permanent(child),
            CatalogRelation::permanent(part),
            CatalogRelation::permanent(leaf),
        ]
    });

    let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
    let relname = required(schema.column_index("relname"), "relname is missing")?;
    let flag = required(
        schema.column_index("relhassubclass"),
        "relhassubclass is missing",
    )?;
    let parents: Vec<String> = rows
        .iter()
        .filter(|r| r[flag] == Value::Bool(true))
        .filter_map(|r| match &r[relname] {
            Value::Text(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(parents, vec!["base".to_string(), "part".to_string()]);
    Ok(())
}

/// `pg_class.relfrozenxid`/`relminmxid` are non-zero for exactly the relations
/// that hold unfrozen tuples — a table and a TOAST relation.
///
/// A **sequence** is the case worth naming: it is heap-backed in PostgreSQL,
/// yet reports `0` because its one tuple is written frozen (verified on
/// PostgreSQL 18.4, where only `relkind` `r` and `t` report non-zero). Grouping
/// it with the tables — which "does it have storage" invites — is what this
/// test exists to catch.
#[test]
fn pg_class_freeze_horizons_are_nonzero_exactly_for_relations_with_tuples() -> anyhow::Result<()> {
    let cat = wide_fixture();
    let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
    let relkind = required(schema.column_index("relkind"), "relkind is missing")?;
    let frozen = required(
        schema.column_index("relfrozenxid"),
        "relfrozenxid is missing",
    )?;
    let minmxid = required(schema.column_index("relminmxid"), "relminmxid is missing")?;

    let mut kinds: Vec<u8> = Vec::new();
    for row in &rows {
        let Value::Char(kind) = row[relkind] else {
            anyhow::bail!("relkind is not a \"char\"");
        };
        let holds_unfrozen_tuples = matches!(kind, b'r' | b't');
        let expected = Value::Xid(if holds_unfrozen_tuples { 1 } else { 0 });
        assert_eq!(row[frozen], expected, "relfrozenxid for relkind {kind}");
        assert_eq!(row[minmxid], expected, "relminmxid for relkind {kind}");
        kinds.push(kind);
    }
    // Without these the loop above proves nothing: it passes trivially on a
    // fixture that happens to hold only one side of the split.
    assert!(kinds.contains(&b'r') && kinds.contains(&b't'), "{kinds:?}");
    assert!(
        kinds.contains(&b'S'),
        "no sequence in the fixture: {kinds:?}"
    );
    assert!(kinds.contains(&b'p') && kinds.contains(&b'i'), "{kinds:?}");
    Ok(())
}

/// `pg_class.relfilenode` reports the storage engine's real file number, and
/// reports `0` for exactly the relations PostgreSQL gives no storage.
///
/// The second half is what upstream's `sanity_check` checks (`relkind IN ('v',
/// 'c', 'f', 'p', 'I') AND relfilenode <> 0` must find nothing), and it is not
/// automatic here: our engine creates a partitioned parent through the same path
/// as any other table, so it *does* hold a file number — the parent below is
/// given one deliberately, so a change that simply forwarded whatever the engine
/// said would fail this test rather than the regress suite.
#[test]
fn pg_class_relfilenode_is_zero_exactly_for_relations_without_storage() -> anyhow::Result<()> {
    use crabgresql_storage_api::{
        Column, IndexConstraint, IndexKey, IndexMetadata, IndexMethod, PartitionScheme,
        PartitionStrategy, RelStats, RelationFilenodes, TableSchema,
    };
    use crabgresql_types::PgType;

    let plain = |name: &str| TableSchema::new(name, vec![Column::new("a", PgType::Int4)]);

    let mut tbl = CatalogRelation::permanent(plain("tbl"));
    tbl.indexes = vec![
        IndexMetadata {
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
        },
        // A metadata-only index: the planner may use it, but it has no file.
        IndexMetadata {
            name: "tbl_meta".to_string(),
            method: IndexMethod::BTree,
            keys: vec![IndexKey {
                column: 0,
                descending: true,
                nulls_first: false,
            }],
            unique: false,
            nulls_distinct: true,
            constraint: None,
        },
    ];
    tbl.toast = Some(RelStats {
        relpages: 3,
        reltuples: 0.0,
        analyzed: false,
        curpages: Some(3),
        columns: std::sync::Arc::from([]),
    });
    tbl.filenodes = RelationFilenodes {
        rel: 41,
        toast: 42,
        indexes: vec![("tbl_pkey".to_string(), 43), ("tbl_meta".to_string(), 0)],
    };

    let mut parent_schema = plain("part");
    parent_schema.partition_scheme = Some(PartitionScheme {
        strategy: PartitionStrategy::Range,
        key_columns: vec![0],
    });
    let mut parent = CatalogRelation::permanent(parent_schema);
    // The file our engine really gives a partitioned parent, and never writes to.
    parent.filenodes = RelationFilenodes {
        rel: 44,
        ..RelationFilenodes::default()
    };

    let mut vw = CatalogRelation::view(plain("vw"), None);
    vw.filenodes = RelationFilenodes::default();

    let mut sq = CatalogRelation::sequence(
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
            last_value: None,
            owned_by: None,
        },
    );
    sq.filenodes.rel = 45;

    let cat =
        SystemCatalog::with_catalog_relations("db", "owner", vec![tbl, parent, vw, sq.clone()]);
    let (schema, rows) = required(cat.build_pg_catalog("pg_class"), "pg_class is missing")?;
    assert!(rows.iter().all(|r| r.len() == schema.columns.len()));

    // PostgreSQL's attnum 8: straight after `relam`, before `reltablespace`.
    let relfilenode = required(schema.column_index("relfilenode"), "relfilenode is missing")?;
    assert_eq!(relfilenode, 7, "relfilenode must sit at PG's attnum 8");
    assert_eq!(
        required(schema.column_index("relam"), "relam is missing")?,
        relfilenode - 1
    );

    let relname = required(schema.column_index("relname"), "relname is missing")?;
    let relkind = required(schema.column_index("relkind"), "relkind is missing")?;
    let node = |name: &str| -> anyhow::Result<Value> {
        required(
            rows.iter()
                .find(|r| r[relname] == Value::Text(name.to_string()))
                .map(|r| r[relfilenode].clone()),
            name,
        )
    };

    assert_eq!(node("tbl")?, Value::Oid(41), "a table names its heap file");
    assert_eq!(node("tbl_pkey")?, Value::Oid(43), "an index names its own");
    assert_eq!(
        node("tbl_meta")?,
        Value::Oid(0),
        "a metadata-only index has no file"
    );
    // Found by relkind: its name carries the parent's positional OID, which this
    // test has no business predicting.
    let toast = required(
        rows.iter()
            .find(|r| r[relkind] == Value::Char(b't'))
            .map(|r| r[relfilenode].clone()),
        "no TOAST relation",
    )?;
    assert_eq!(toast, Value::Oid(42));
    assert_eq!(
        node("sq")?,
        Value::Oid(45),
        "a sequence is a relation with storage in PG, so 0 would be wrong"
    );
    assert_eq!(
        node("part")?,
        Value::Oid(0),
        "a partitioned parent has no storage, whatever file the engine gave it"
    );
    assert_eq!(node("vw")?, Value::Oid(0));

    // The upstream `sanity_check` predicate itself, evaluated over every row.
    let without_storage: Vec<_> = rows
        .iter()
        .filter(|r| matches!(&r[relkind], Value::Char(k) if b"vcfpI".contains(k)))
        .filter(|r| r[relfilenode] != Value::Oid(0))
        .map(|r| r[relname].clone())
        .collect();
    assert!(
        without_storage.is_empty(),
        "relations without storage must not have a relfilenode: {without_storage:?}"
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
    let name = required(schema.column_index("attname"), "attname is missing")?;
    // Not `rows[0]`: the system attributes lead every relation's rows.
    let v = required(
        rows.iter()
            .find(|r| r[name] == Value::Text("v".to_string())),
        "the declared column is missing",
    )?;
    assert_eq!(v[i], Value::Int4(i32::MAX));

    Ok(())
}

/// Each registry table is sorted by `(namespace, name)`, which is what
/// `registry::lookup`'s binary search assumes. Out of order it would not error —
/// it would silently fail to find a relation that is right there.
///
/// Names are unique across *both* tables, not within each: the two are one
/// namespace to every caller, and a name in both would make which definition
/// answers depend on the search order inside `lookup`.
#[test]
fn the_registry_is_sorted_and_its_names_are_unique() {
    for (label, keys) in [
        (
            "CATALOG_RELATIONS",
            registry::CATALOG_RELATIONS
                .iter()
                .map(|def| (def.namespace, def.name))
                .collect::<Vec<_>>(),
        ),
        (
            "CATALOG_VIEWS",
            registry::CATALOG_VIEWS
                .iter()
                .map(|def| (def.rel.namespace, def.rel.name))
                .collect::<Vec<_>>(),
        ),
    ] {
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(keys, sorted, "{label} must be sorted and unique");
    }

    let mut names: Vec<_> = registry::all()
        .map(|def| (def.namespace, def.name))
        .collect();
    let total = names.len();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), total, "a name is served by both tables");

    // ...and the search really does find every entry, in both directions.
    for def in registry::all() {
        assert!(registry::lookup(def.namespace, def.name).is_some());
    }
    assert!(registry::lookup(CatalogNamespace::PgCatalog, "no_such_catalog").is_none());
}

/// A view carries its definition and a base table does not — the whole point of
/// the split.
#[test]
fn only_views_have_a_definition() {
    let pg_views = required(
        builtin_view_definition("pg_views"),
        "pg_views is served as a view and must carry its definition",
    )
    .expect("a served view without a definition");
    assert!(
        pg_views.contains("c.relkind = 'v'"),
        "pg_views must be defined over relkind = 'v': {pg_views}"
    );

    assert_eq!(builtin_view_definition("pg_class"), None);
    assert_eq!(builtin_view_definition("no_such_catalog"), None);
}

/// What keeps the definition text from being decoration. Nothing runs it — the
/// rows come from Rust — so a definition pasted under the wrong name, or a row
/// builder that quietly serves fewer columns than PostgreSQL's view does, has
/// nothing else to fail against.
#[test]
fn every_view_definition_parses_and_names_our_columns() {
    use crabgresql_parser::ast::{Expr, SelectItem, SetExpr, Statement};
    use crabgresql_parser::dialect::PostgreSqlDialect;
    use crabgresql_parser::parser::Parser;

    /// The name a select item publishes. PostgreSQL's own deparse aliases every
    /// item whose name would not otherwise come out right, so a `None` here is
    /// a definition transcribed wrong rather than a shape to handle.
    fn output_name(item: &SelectItem) -> Option<String> {
        match item {
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
            SelectItem::UnnamedExpr(Expr::Identifier(ident)) => Some(ident.value.clone()),
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                parts.last().map(|ident| ident.value.clone())
            }
            _ => None,
        }
    }

    // Checked against an expected set below rather than skipped where they
    // occur: a silent `continue` would outlive the gap that justifies it.
    let mut unparsed = Vec::new();

    for def in registry::CATALOG_VIEWS {
        let name = def.rel.name;
        let Ok(statements) = Parser::parse_sql(&PostgreSqlDialect {}, def.definition) else {
            unparsed.push(name);
            continue;
        };
        let [Statement::Query(query)] = statements.as_slice() else {
            panic!("{name}: a definition must be one query");
        };
        // Every one of PostgreSQL's own definitions is a plain SELECT or a
        // UNION of them; the left-most arm names the columns either way.
        let mut body = query.body.as_ref();
        while let SetExpr::SetOperation { left, .. } = body {
            body = left.as_ref();
        }
        let SetExpr::Select(select) = body else {
            panic!("{name}: a definition must select");
        };
        let selected: Vec<_> = select
            .projection
            .iter()
            .map(|item| {
                output_name(item)
                    .unwrap_or_else(|| panic!("{name}: cannot name select item {item}"))
            })
            .collect();
        let published: Vec<_> = (def.rel.schema)()
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect();
        assert_eq!(
            published, selected,
            "{name}: the row builder publishes columns the definition does not select"
        );
    }

    // Every definition parses, so every one of them had its columns checked
    // above. The list stays so that a transcription reaching for grammar this
    // build lacks fails here rather than quietly skipping that check.
    assert!(
        unparsed.is_empty(),
        "definitions this build cannot parse: {unparsed:?}"
    );
}

/// Every `pg_catalog` relation has a distinct, non-zero OID that round-trips,
/// and none of them lands in a band some other OID space owns.
///
/// `CatalogRelDef::xmin_column` names a column by string, so nothing but this
/// keeps it in step with the schema it names. A typo does not fail: the relation
/// quietly falls back to the catalog-wide generation, and every row of it starts
/// reporting the same `xmin` again — the exact behavior the field exists to
/// replace, with nothing to notice it by.
#[test]
fn xmin_columns_exist_and_are_oids() {
    for def in registry::all() {
        let Some(name) = def.xmin_column else {
            continue;
        };
        let schema = (def.schema)();
        let index = required(
            schema.column_index(name),
            &format!("{} has no column {name}", def.name),
        )
        .expect("the registry names a column its schema does not have");
        assert_eq!(
            schema.columns[index].ty,
            crabgresql_types::PgType::Oid,
            "{}.{name} must hold a relation OID",
            def.name,
        );
    }
}

/// A hand-typed OID is the one thing in the registry no compiler checks. A
/// digit slipped into a synthetic band would make `'pg_class'::regclass`
/// name a user relation — or two catalogs answer to the same cast.
#[test]
fn catalog_oids_are_unique_and_outside_every_synthetic_band() {
    let mut seen = std::collections::HashSet::new();
    for def in registry::all() {
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
    for def in registry::all() {
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
                    owned_by: None,
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
            domain: None,
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
            variadic_elem: 0,
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
    for def in registry::all() {
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
        "pg_depend",
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
                    owned_by: None,
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
    let attnum = required(aschema.column_index("attnum"), "attnum")?;
    // Declared columns only: a TOAST relation is an ordinary heap upstream, so
    // it also publishes the six system attributes at negative `attnum`.
    let names: Vec<String> = arows
        .iter()
        .filter(|r| r[attrelid] == Value::Oid(toast_oid))
        .filter(|r| matches!(r[attnum], Value::Int2(n) if n > 0))
        .map(|r| match &r[attname] {
            Value::Text(s) => s.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["chunk_id", "chunk_seq", "chunk_data"]);
    assert_eq!(
        arows
            .iter()
            .filter(|r| r[attrelid] == Value::Oid(toast_oid))
            .filter(|r| matches!(r[attnum], Value::Int2(n) if n < 0))
            .count(),
        6,
    );

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
            ("pg_conversion", 98),
            ("pg_extension", 1),
            // Three from the `.dat`, plus `plpgsql`.
            ("pg_language", 4),
            ("pg_namespace", 3),
            // Every operator upstream ships; each one carries a `descr`.
            ("pg_operator", 805),
            // Every function the generated catalogs reference and upstream
            // wrote a `descr` for. It grew with `pg_amproc`, `pg_operator` and
            // `pg_aggregate`: a support function, an `oprcode` or a transition
            // function is a `pg_proc` row like any other, and its description
            // comes along, as `domain_in`/`domain_recv` did with domains.
            ("pg_proc", 1311),
            // The `simple` configuration and dictionary and the default parser
            // come from the `.dat`; the other 29 of each are snowball's, whose
            // comments `initdb` writes with `COMMENT ON`.
            ("pg_ts_config", 30),
            ("pg_ts_dict", 30),
            ("pg_ts_parser", 1),
            // Four templates from the `.dat`, plus snowball.
            ("pg_ts_template", 5),
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

/// Which sequence a stored default names — the one link between a `serial`
/// column and its sequence, so a miss here silently costs two `pg_depend`
/// edges, `pg_get_serial_sequence`, and `DROP SEQUENCE`'s DETAIL.
#[test]
fn nextval_target_reads_the_call_and_not_the_word() {
    let target = |sql: &str| nextval_target(sql).map(|(ns, name)| (ns.unwrap_or_default(), name));
    let unqualified = |name: &str| Some((String::new(), name.to_string()));

    assert_eq!(
        target("nextval('t_id_seq'::regclass)"),
        unqualified("t_id_seq")
    );
    assert_eq!(
        target("nextval('app.t_id_seq'::regclass)"),
        Some(("app".to_string(), "t_id_seq".to_string()))
    );
    // Quoting is what keeps a mixed-case sequence findable; an unquoted part
    // folds, because that is what re-parsing the text would do to it.
    assert_eq!(
        target("nextval('\"MixT_Id_seq\"'::regclass)"),
        unqualified("MixT_Id_seq")
    );
    assert_eq!(
        target("nextval('T_ID_SEQ'::regclass)"),
        unqualified("t_id_seq")
    );
    // A doubled quote inside the literal is one quote in the name.
    assert_eq!(
        target("nextval('\"it''s_id_seq\"'::regclass)"),
        unqualified("it's_id_seq")
    );
    // PostgreSQL's own function, however it is spelled.
    assert_eq!(
        target("pg_catalog.nextval('s'::regclass)"),
        unqualified("s")
    );

    // A different function whose name merely ends in `nextval`.
    assert_eq!(target("my_nextval('s'::regclass)"), None);
    assert_eq!(target("app.nextval('s'::regclass)"), None);
    // The word inside a string constant names nothing.
    assert_eq!(target("('nextval' || substr('abc', 1, 2))"), None);
    assert_eq!(target("'nextval(''s'')'::text"), None);
    // Neither does a call with a non-literal argument, or an unterminated one.
    assert_eq!(target("nextval(seq_of(x))"), None);
    assert_eq!(target("nextval('unterminated"), None);
}

/// The class OIDs `pg_depend` labels an object with are the OIDs those
/// relations are actually served under, so `classid::regclass` round-trips to
/// the catalog's name — which is how every query in the smoke suite (and
/// `pg_dump`) reads a dependency row.
#[test]
fn depend_class_oids_are_the_relations_they_name() {
    for (oid, name) in [
        (oids::PG_TYPE_CLASS_OID, "pg_type"),
        (oids::PG_PROC_CLASS_OID, "pg_proc"),
        (oids::PG_CLASS_CLASS_OID, "pg_class"),
        (oids::PG_ATTRDEF_CLASS_OID, "pg_attrdef"),
        (oids::PG_CONSTRAINT_CLASS_OID, "pg_constraint"),
        (oids::PG_NAMESPACE_CLASS_OID, "pg_namespace"),
        (oids::PG_REWRITE_CLASS_OID, "pg_rewrite"),
    ] {
        assert_eq!(builtin_relation_oid(name), Some(oid), "{name}");
    }
}

/// A snapshot holding one of every shape that produces a `pg_depend` edge.
///
/// Modelled on a fixture built on PostgreSQL 18.4 to read its answer off a live
/// server: `CREATE TYPE mood AS ENUM …; CREATE TABLE t (id serial PRIMARY KEY,
/// x text DEFAULT 'q', m mood, y int CHECK (y > 0)); CREATE VIEW v AS SELECT
/// id, x FROM t; CREATE TABLE child () INHERITS (t)`, with `t` large enough to
/// have a TOAST relation.
fn depend_fixture() -> SystemCatalog {
    use crabgresql_storage_api::{CheckConstraint, IndexKey, IndexMethod, InheritParent};

    const MOOD_OID: u32 = 16_500;
    let mut t = TableSchema::new(
        "t",
        vec![
            Column::new("id", PgType::Int4),
            Column::new("x", PgType::Text),
            Column::new("m", PgType::User(MOOD_OID)),
            Column::new("y", PgType::Int4),
        ],
    );
    t.columns[0].default = Some("nextval('t_id_seq'::regclass)".to_string());
    t.columns[1].default = Some("'q'::text".to_string());
    t.checks = vec![CheckConstraint {
        name: "t_y_check".to_string(),
        expr: "(y > 0)".to_string(),
        columns: vec![3],
        validated: true,
        islocal: true,
        inhcount: 0,
    }];
    let key = |column: usize| IndexKey {
        column,
        descending: false,
        nulls_first: false,
    };
    let mut table = CatalogRelation::permanent(t);
    table.indexes = vec![
        IndexMetadata {
            name: "t_pkey".to_string(),
            method: IndexMethod::BTree,
            keys: vec![key(0)],
            unique: true,
            nulls_distinct: true,
            constraint: Some(IndexConstraint::PrimaryKey),
        },
        // Keyed on the same column twice, which PostgreSQL accepts and records
        // one dependency for.
        IndexMetadata {
            name: "t_x_idx".to_string(),
            method: IndexMethod::BTree,
            keys: vec![key(1), key(1)],
            unique: false,
            nulls_distinct: true,
            constraint: None,
        },
    ];
    table.toast = Some(RelStats {
        relpages: 1,
        reltuples: 0.0,
        analyzed: false,
        curpages: Some(1),
        columns: std::sync::Arc::from([]),
    });

    let mut child = TableSchema::new("child", vec![Column::new("id", PgType::Int4)]);
    child.inherits = vec![InheritParent {
        namespace: "public".to_string(),
        name: "t".to_string(),
    }];

    SystemCatalog::from_source(Arc::new(
        StaticSource::new(vec![
            table,
            CatalogRelation::permanent(child),
            CatalogRelation::view(
                TableSchema::new(
                    "v",
                    vec![
                        Column::new("id", PgType::Int4),
                        Column::new("x", PgType::Text),
                    ],
                ),
                Some("SELECT t.id, t.x FROM t".to_string()),
            ),
            CatalogRelation::sequence(
                "t_id_seq",
                "public",
                CatalogSequence {
                    type_oid: PgType::Int8.oid(),
                    start: 1,
                    increment: 1,
                    min: 1,
                    max: i64::MAX,
                    cache: 1,
                    cycle: false,
                    last_value: None,
                    owned_by: Some("t".to_string()),
                },
            ),
        ])
        .user_types(vec![CatalogUserType {
            oid: MOOD_OID,
            name: "mood".to_string(),
            enum_labels: Some(vec!["sad".to_string(), "ok".to_string()]),
            domain: None,
        }])
        .view_dependencies(vec![CatalogViewDependency {
            namespace: "public".to_string(),
            name: "v".to_string(),
            reads: vec![ViewDepRelation {
                namespace: "public".to_string(),
                name: "t".to_string(),
                columns: Some(vec!["id".to_string(), "x".to_string()]),
            }],
        }]),
    ))
}

/// `pg_depend` records the same graph PostgreSQL records for the same schema.
///
/// Asserted as *named* edges rather than OIDs: the numbers are assigned per
/// snapshot (see [`SystemCatalog::relation_oids`]), so pinning them would test
/// the numbering rather than the graph, and a named edge is also what the
/// PostgreSQL 18.4 output this list was read off looks like.
#[test]
fn pg_depend_records_the_graph_postgres_records() -> anyhow::Result<()> {
    let cat = depend_fixture();
    let (schema, rows) = required(cat.build_pg_catalog("pg_depend"), "pg_depend is missing")?;
    let col = |row: &[Value], name: &str| -> anyhow::Result<Value> {
        Ok(row[required(schema.column_index(name), name)?].clone())
    };

    // Name every object the graph can point at, so an edge reads as text.
    let mut names: std::collections::HashMap<(u32, u32), String> = std::collections::HashMap::new();
    for (class, relation, key, label) in [
        (oids::PG_CLASS_CLASS_OID, "pg_class", "oid", "relname"),
        (
            oids::PG_CONSTRAINT_CLASS_OID,
            "pg_constraint",
            "oid",
            "conname",
        ),
        (
            oids::PG_NAMESPACE_CLASS_OID,
            "pg_namespace",
            "oid",
            "nspname",
        ),
        (oids::PG_TYPE_CLASS_OID, "pg_type", "oid", "typname"),
    ] {
        let (s, rows) = required(cat.build_pg_catalog(relation), relation)?;
        let (oid, label) = (
            required(s.column_index(key), key)?,
            required(s.column_index(label), label)?,
        );
        for row in rows {
            if let (Value::Oid(o), Value::Text(name)) = (&row[oid], &row[label]) {
                names.insert((class, *o), name.clone());
            }
        }
    }
    // The two catalogs whose rows have no name of their own are labelled by
    // what they belong to, which is how a reader identifies them anyway.
    let (s, attrdefs) = required(cat.build_pg_catalog("pg_attrdef"), "pg_attrdef")?;
    for row in attrdefs {
        let oid = required(s.column_index("oid"), "oid")?;
        let adrelid = required(s.column_index("adrelid"), "adrelid")?;
        let adnum = required(s.column_index("adnum"), "adnum")?;
        if let (Value::Oid(o), Value::Oid(rel), Value::Int2(num)) =
            (&row[oid], &row[adrelid], &row[adnum])
        {
            let table = names
                .get(&(oids::PG_CLASS_CLASS_OID, *rel))
                .cloned()
                .unwrap_or_default();
            names.insert(
                (oids::PG_ATTRDEF_CLASS_OID, *o),
                format!("default({table}.{num})"),
            );
        }
    }
    let (s, rules) = required(cat.build_pg_catalog("pg_rewrite"), "pg_rewrite")?;
    for row in rules {
        let oid = required(s.column_index("oid"), "oid")?;
        let ev_class = required(s.column_index("ev_class"), "ev_class")?;
        if let (Value::Oid(o), Value::Oid(view)) = (&row[oid], &row[ev_class]) {
            let view = names
                .get(&(oids::PG_CLASS_CLASS_OID, *view))
                .cloned()
                .unwrap_or_default();
            names.insert((oids::PG_REWRITE_CLASS_OID, *o), format!("_RETURN({view})"));
        }
    }

    let object = |class: &Value, oid: &Value, sub: &Value| -> String {
        let (Value::Oid(class), Value::Oid(oid), Value::Int4(sub)) = (class, oid, sub) else {
            return "?".to_string();
        };
        let name = names
            .get(&(*class, *oid))
            .cloned()
            .unwrap_or_else(|| format!("oid {oid}"));
        match sub {
            0 => name,
            n => format!("{name}.{n}"),
        }
    };
    let mut edges = Vec::new();
    for row in &rows {
        let deptype = match col(row, "deptype")? {
            Value::Char(c) => c as char,
            other => anyhow::bail!("deptype is {other:?}"),
        };
        edges.push(format!(
            "{} -> {} {deptype}",
            object(
                &col(row, "classid")?,
                &col(row, "objid")?,
                &col(row, "objsubid")?
            ),
            object(
                &col(row, "refclassid")?,
                &col(row, "refobjid")?,
                &col(row, "refobjsubid")?
            ),
        ));
    }
    edges.sort();

    // The TOAST relation is named after its table's OID, which this test does
    // not pin, so its name is read back from `pg_class`.
    let toast = required(
        names
            .iter()
            .find(|((class, _), name)| {
                *class == oids::PG_CLASS_CLASS_OID && name.starts_with("pg_toast_")
            })
            .map(|(_, name)| name.clone()),
        "no TOAST relation in the fixture",
    )?;
    let mut expected: Vec<String> = [
        // Every relation belongs to its schema; an index and a TOAST relation
        // do not carry the edge, as in PostgreSQL.
        "t -> public n",
        "child -> public n",
        "v -> public n",
        "t_id_seq -> public n",
        "mood -> public n",
        // The inheritance child is *not* dropped with its parent, so `n`.
        "child -> t n",
        // A user type is depended on; `text` and `int4` are pinned and are not.
        "t.3 -> mood n",
        // The index backs a constraint, so it depends on the constraint; the
        // constraint depends on the column.
        "t_pkey -> t_pkey i",
        "t_pkey -> t.1 a",
        // A plain index depends on the column it keys — once, however many times
        // the key list names it.
        "t_x_idx -> t.2 a",
        // A CHECK contributes both rows per column it reads.
        "t_y_check -> t.4 a",
        "t_y_check -> t.4 n",
        // The `serial` pair: the sequence onto the column, the default onto the
        // sequence. Plus the ordinary default edge of the second column.
        "t_id_seq -> t.1 a",
        "default(t.1) -> t.1 a",
        "default(t.1) -> t_id_seq n",
        "default(t.2) -> t.2 a",
        // The TOAST relation has no life apart from its table.
        "<toast> -> t i",
        // The view's rule: internally on the view, by column on what it reads.
        "_RETURN(v) -> v i",
        "_RETURN(v) -> t.1 n",
        "_RETURN(v) -> t.2 n",
    ]
    .iter()
    .map(|edge| edge.replace("<toast>", &toast))
    .collect();
    expected.sort();

    assert_eq!(edges, expected);
    Ok(())
}
