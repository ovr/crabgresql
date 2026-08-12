//! Generated columns as the binder sees them: a virtual one substitutes its
//! expression wherever the row is read, and every write path refuses a value.

use super::common::*;
use crabgresql_storage_api::{GeneratedColumn, Generation};

/// A table with one stored and one virtual generated column, next to two plain
/// ones.
fn engine_with_generated() -> anyhow::Result<Arc<dyn TableEngine>> {
    let generated = |name: &str, kind: Generation, expr: &str| {
        let mut c = Column::new(name, PgType::Int4);
        c.generated = Some(GeneratedColumn {
            kind,
            expr: expr.to_string(),
        });
        c
    };
    let engine = crabgresql_pg_engine::ephemeral_engine();
    engine
        .create_table(TableSchema::new(
            "g",
            vec![
                Column::new("a", PgType::Int4),
                generated("s", Generation::Stored, "(a * 2)"),
                generated("v", Generation::Virtual, "(a + 1)"),
            ],
        ))
        .context("creating the generated-column test table")?;
    Ok(engine)
}

fn bind(sql: &str) -> anyhow::Result<LogicalPlan> {
    let engine = engine_with_generated()?;
    let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
    let stmts = crabgresql_parser::parse(sql)
        .map_err(|error| anyhow!("invalid SQL test fixture `{sql}`: {error}"))?;
    let plan = match &stmts[0] {
        ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
        ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
        ast::Statement::Update(u) => bind_update(&engine, &catalog, u),
        other => bail!("unexpected statement in test fixture: {other}"),
    };
    plan.map_err(anyhow::Error::new)
        .with_context(|| format!("binding `{sql}`"))
}

fn err(sql: &str) -> anyhow::Result<BindError> {
    match bind(sql) {
        Ok(_) => bail!("expected a bind error for `{sql}`"),
        Err(error) => error
            .downcast::<BindError>()
            .with_context(|| format!("`{sql}` failed before the binder ran")),
    }
}

/// A virtual column reads as its expression; a stored one reads as its slot,
/// because the write path put a value there.
#[test]
fn a_virtual_column_binds_to_its_expression() -> anyhow::Result<()> {
    let QueryPlan { projections, .. } = bind("SELECT v, s FROM g")?.into_query()?;
    assert!(
        matches!(&projections[0], BoundExpr::Binary { op: BinOp::Add, .. }),
        "a virtual column must substitute its expression, got {:?}",
        projections[0]
    );
    assert!(
        matches!(&projections[1], BoundExpr::ColumnRef { index: 1, .. }),
        "a stored column is read from the row, got {:?}",
        projections[1]
    );
    Ok(())
}

/// `*` expands the same way a named reference resolves — otherwise `SELECT *`
/// would answer NULL for every virtual column.
#[test]
fn a_wildcard_substitutes_a_virtual_column_too() -> anyhow::Result<()> {
    let QueryPlan { projections, .. } = bind("SELECT * FROM g")?.into_query()?;
    assert!(matches!(
        &projections[0],
        BoundExpr::ColumnRef { index: 0, .. }
    ));
    assert!(matches!(
        &projections[1],
        BoundExpr::ColumnRef { index: 1, .. }
    ));
    assert!(
        matches!(&projections[2], BoundExpr::Binary { op: BinOp::Add, .. }),
        "`*` must substitute a virtual column, got {:?}",
        projections[2]
    );
    Ok(())
}

/// In a join the substituted expression is rebased onto the combined row, so it
/// reads the *right* relation's columns.
#[test]
fn a_substituted_expression_is_rebased_in_a_join() -> anyhow::Result<()> {
    let JoinPlan { projections, .. } =
        bind("SELECT y.v FROM g x JOIN g y ON x.a = y.a")?.into_join()?;
    let BoundExpr::Binary { left, .. } = &projections[0] else {
        anyhow::bail!(
            "expected the substituted expression, got {:?}",
            projections[0]
        );
    };
    assert!(
        matches!(left.as_ref(), BoundExpr::ColumnRef { index: 3, .. }),
        "the right arm's `a` sits at index 3 of the combined row, got {left:?}",
    );
    Ok(())
}

#[test]
fn writing_a_generated_column_is_refused() -> anyhow::Result<()> {
    for (sql, message) in [
        (
            "INSERT INTO g VALUES (1, 2)",
            "cannot insert a non-DEFAULT value into column \"s\"",
        ),
        (
            "INSERT INTO g (a, s) VALUES (1, 2)",
            "cannot insert a non-DEFAULT value into column \"s\"",
        ),
        (
            "INSERT INTO g (a, v) SELECT 1, 2",
            "cannot insert a non-DEFAULT value into column \"v\"",
        ),
        (
            "UPDATE g SET s = 2",
            "column \"s\" can only be updated to DEFAULT",
        ),
        (
            "UPDATE g SET v = 2",
            "column \"v\" can only be updated to DEFAULT",
        ),
    ] {
        let error = err(sql)?;
        assert_eq!(error.code, "428C9", "{sql}");
        assert_eq!(error.message, message, "{sql}");
        assert_eq!(
            error.detail.as_deref(),
            Some(
                format!(
                    "Column \"{}\" is a generated column.",
                    message.split('"').nth(1).unwrap_or_default()
                )
                .as_str()
            ),
            "{sql}"
        );
    }
    Ok(())
}

/// DEFAULT is the one value a statement may write, and it means "compute it":
/// the slot binds to a NULL placeholder the executor overwrites.
#[test]
fn default_is_accepted_for_a_generated_column() -> anyhow::Result<()> {
    let InsertPlan { source, .. } =
        bind("INSERT INTO g VALUES (1, DEFAULT, DEFAULT)")?.into_insert()?;
    let InsertSource::Values(rows) = source else {
        anyhow::bail!("expected a VALUES source");
    };
    assert!(matches!(
        &rows[0][1],
        BoundExpr::Const {
            value: Value::Null,
            ..
        }
    ));
    assert!(matches!(
        &rows[0][2],
        BoundExpr::Const {
            value: Value::Null,
            ..
        }
    ));
    Ok(())
}
