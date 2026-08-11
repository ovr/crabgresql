//! `COPY FROM`: field parsing, typmods and deferred defaults.

use super::common::*;

/// Bind a `COPY … FROM stdin` the way the server does, so a test starts from
/// the same `CopyFromPlan` a real load builds its rows against.
fn copy_plan(engine: &Arc<dyn TableEngine>, sql: &str) -> anyhow::Result<CopyFromPlan> {
    let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
    let stmts = crabgresql_parser::parse(sql)
        .map_err(|error| anyhow!("invalid SQL test fixture `{sql}`: {error}"))?;
    let ast::Statement::Copy {
        source,
        to,
        target,
        options,
        legacy_options,
        ..
    } = &stmts[0]
    else {
        bail!("expected a COPY statement: {sql}");
    };
    bind_copy_from(
        engine,
        &catalog,
        source,
        *to,
        target,
        options,
        legacy_options,
    )
    .with_context(|| format!("binding `{sql}`"))
}

fn copy_rows(plan: &CopyFromPlan, rows: Vec<Vec<Option<String>>>) -> anyhow::Result<InsertSource> {
    let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
    let InsertPlan { source, .. } = plan
        .build_insert(&catalog, &FmtCtx::utc_default(), rows)
        .context("building the COPY rows")?
        .into_insert()?;
    Ok(source)
}

fn field(text: &str) -> Option<String> {
    Some(text.to_string())
}

/// An ordinary relation loads straight into values: no expression tree is
/// built only to be torn down again by the executor. This is the whole point
/// of the fast path, so it is asserted rather than left to the benchmark.
#[test]
fn copy_parses_fields_straight_into_values() -> anyhow::Result<()> {
    let engine = engine_with_table()?;
    let plan = copy_plan(&engine, "COPY t FROM stdin")?;
    let source = copy_rows(
        &plan,
        vec![vec![field("7"), field("8"), field("hi"), field("t")]],
    )?;
    let InsertSource::Tuples { rows, defaults } = source else {
        bail!("expected a Tuples source");
    };
    assert!(defaults.is_empty(), "no column here has a default to defer");
    assert_eq!(
        rows,
        vec![vec![
            Value::Int4(7),
            Value::Int8(8),
            Value::Text("hi".into()),
            Value::Bool(true),
        ]]
    );
    Ok(())
}

/// Every `parse_unknown` arm must return exactly the `Value` variant its
/// `PgType` pairs with, because `apply_column_typmod` destructures on that
/// pairing and raises an internal error when it does not hold.
///
/// There is no second path to absorb a mismatch any more, so the invariant
/// is pinned here rather than assumed: a future arm that widened a
/// representation (say `money` carried as `Value::Numeric`) would turn every
/// COPY into that column into an `XX000` while the equivalent INSERT kept
/// working, and nothing else in the suite would notice.
#[test]
fn every_column_type_parses_into_the_shape_its_typmod_expects() -> anyhow::Result<()> {
    use crabgresql_types::{Numeric, oid};

    // (type, typmod, a literal that type accepts)
    let cases: Vec<(PgType, i32, &str)> = vec![
        (PgType::Bool, -1, "t"),
        (PgType::Char, -1, "a"),
        (PgType::Int2, -1, "1"),
        (PgType::Int4, -1, "1"),
        (PgType::Int8, -1, "1"),
        (PgType::Oid, -1, "1"),
        (PgType::Float4, -1, "1.5"),
        (PgType::Float8, -1, "1.5"),
        (PgType::Numeric, -1, "1.5"),
        (PgType::Numeric, Numeric::pack_typmod(5, 2), "1.5"),
        (PgType::Text, -1, "abc"),
        (PgType::Varchar, -1, "abc"),
        (PgType::Varchar, 5, "abc"),
        (PgType::Bpchar, -1, "abc"),
        (PgType::Bpchar, 5, "abc"),
        (PgType::Name, -1, "abc"),
        (PgType::Bytea, -1, r"\x4142"),
        (PgType::Uuid, -1, "00000000-0000-0000-0000-000000000001"),
        (PgType::Date, -1, "2020-01-01"),
        (PgType::Time, -1, "01:02:03"),
        (PgType::Time, 2, "01:02:03.456"),
        (PgType::TimeTz, -1, "01:02:03+00"),
        (PgType::TimeTz, 2, "01:02:03.456+00"),
        (PgType::Timestamp, -1, "2020-01-01 01:02:03"),
        (PgType::Timestamp, 2, "2020-01-01 01:02:03.456"),
        (PgType::TimestampTz, -1, "2020-01-01 01:02:03+00"),
        (PgType::TimestampTz, 2, "2020-01-01 01:02:03.456+00"),
        (PgType::Interval, -1, "1 day"),
        (PgType::Interval, 2, "1.456 seconds"),
        (PgType::Bit, -1, "1010"),
        (PgType::Bit, 4, "1010"),
        (PgType::Varbit, -1, "1010"),
        (PgType::Varbit, 4, "1010"),
        (PgType::Inet, -1, "10.0.0.1"),
        (PgType::Cidr, -1, "10.0.0.0/8"),
        (PgType::Macaddr, -1, "08:00:2b:01:02:03"),
        (PgType::Macaddr8, -1, "08:00:2b:01:02:03:04:05"),
        (PgType::Money, -1, "$1.00"),
        (PgType::Json, -1, r#"{"a":1}"#),
        (PgType::Jsonb, -1, r#"{"a":1}"#),
        (PgType::Jsonpath, -1, "$.a"),
        (PgType::Tsvector, -1, "a b"),
        (PgType::Tsquery, -1, "a & b"),
        (PgType::Xid, -1, "1"),
        (PgType::Xid8, -1, "1"),
        (PgType::Tid, -1, "(0,1)"),
        (PgType::PgLsn, -1, "0/0"),
        (PgType::Point, -1, "(1,2)"),
        (PgType::Lseg, -1, "[(1,2),(3,4)]"),
        (PgType::Box, -1, "(1,2),(3,4)"),
        (PgType::Path, -1, "[(1,2),(3,4)]"),
        (PgType::Line, -1, "{1,2,3}"),
        (PgType::Circle, -1, "<(1,2),3>"),
        (PgType::Polygon, -1, "((1,2),(3,4),(5,6))"),
        (PgType::Array(oid::INT4), -1, "{1,2}"),
        (PgType::Array(oid::TEXT), -1, "{a,b}"),
        (
            PgType::Array(oid::TIMESTAMPTZ),
            -1,
            "{2020-01-01 00:00:00+00}",
        ),
    ];

    let fmt = FmtCtx::utc_default();
    for (ty, typmod, literal) in cases {
        let column = Column::with_typmod("c", ty, typmod);
        let value = match crate::expr::parse_unknown(literal, ty, &fmt) {
            Ok(value) => value,
            Err(e) => bail!("{ty:?} typmod {typmod} rejected {literal:?}: {e}"),
        };
        if let Err(e) = apply_column_typmod(value, &column) {
            bail!(
                "{ty:?} typmod {typmod}: parse_unknown and apply_column_typmod \
                 disagree on the value shape for {literal:?}: {e}"
            );
        }
    }
    Ok(())
}

/// A default that cannot fold — `nextval` on a `serial`, `now()`, a routine
/// — is handed to the executor to run once per row, not baked into the
/// template. Getting this wrong would give every row the same sequence
/// value.
#[test]
fn copy_defers_a_default_that_does_not_fold() -> anyhow::Result<()> {
    let engine = crabgresql_pg_engine::ephemeral_engine();
    let mut stamped = Column::new("stamped", PgType::Timestamp);
    stamped.default = Some("now()".into());
    let mut literal = Column::new("literal", PgType::Int4);
    literal.default = Some("7".into());
    if let Err(error) = engine.create_table(TableSchema::new(
        "d",
        vec![Column::new("id", PgType::Int4), stamped, literal],
    )) {
        bail!("failed to create the default test table: {error}");
    }
    let engine: Arc<dyn TableEngine> = engine;
    let plan = copy_plan(&engine, "COPY d (id) FROM stdin")?;
    let InsertSource::Tuples { rows, defaults } = copy_rows(&plan, vec![vec![field("1")]])? else {
        bail!("expected a Tuples source");
    };
    assert_eq!(
        defaults.len(),
        1,
        "only the volatile default needs per-row evaluation"
    );
    assert_eq!(defaults[0].0, 1, "and it is the `stamped` column");
    // The constant default is already in the row; the deferred slot holds a
    // placeholder the executor overwrites.
    assert_eq!(rows[0][0], Value::Int4(1));
    assert_eq!(rows[0][1], Value::Null);
    assert_eq!(rows[0][2], Value::Int4(7));
    Ok(())
}

/// `name` truncates whatever its typmod says, and a `name` column's is
/// always -1 — so a value-level typmod keyed on `typmod >= 0` would silently
/// store the untruncated string, and disagree with an ordinary INSERT.
///
/// Asserted in bytes, and with a multibyte value, because that is the only
/// input that can tell `name`'s byte limit from `varchar(n)`'s character
/// one — an ASCII string satisfies both.
#[test]
fn copy_truncates_a_name_column_despite_its_absent_typmod() -> anyhow::Result<()> {
    let column = Column::new("n", PgType::Name);
    assert_eq!(column.typmod, -1);
    let stored = |s: String| -> anyhow::Result<String> {
        match apply_column_typmod(Value::Text(s), &column) {
            Ok(Value::Text(stored)) => Ok(stored),
            other => bail!("a name column must accept text, got {other:?}"),
        }
    };
    assert_eq!(stored("x".repeat(100))?.len(), 63);
    let multibyte = stored("é".repeat(70))?;
    assert_eq!(multibyte.len(), 62, "clipped on a character boundary");
    assert_eq!(multibyte.chars().count(), 31);
    Ok(())
}

/// A NULL never reaches a length rule: it is a NULL in a `char(5)` column,
/// not five blanks.
#[test]
fn copy_leaves_a_null_untouched_by_a_typmod() -> anyhow::Result<()> {
    // `Column::typmod` is the bare declared length; it is `atttypmod()` that
    // adds the varlena header PostgreSQL's catalog reports.
    let column = Column::with_typmod("c", PgType::Bpchar, 5);
    match apply_column_typmod(Value::Null, &column) {
        Ok(Value::Null) => {}
        other => bail!("a NULL must pass through a char(5) typmod: {other:?}"),
    }
    Ok(())
}
