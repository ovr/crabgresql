//! Array subscripting — `a[1]`, its qualified form `t.a[1]`, and the
//! multi-dimensional chain `a[1][2]` — plus the nested `ARRAY[[…]]` constructor.

use super::common::*;

fn engine_with_arrays() -> anyhow::Result<Arc<dyn TableEngine>> {
    let engine = crabgresql_pg_engine::ephemeral_engine();
    engine
        .create_table(TableSchema::new(
            "arr",
            vec![
                Column::new("id", PgType::Int4),
                Column::new("a", PgType::Array(crabgresql_types::oid::INT4)),
            ],
        ))
        .context("creating the array test table")?;
    Ok(engine)
}

fn bind(sql: &str) -> anyhow::Result<LogicalPlan> {
    let engine = engine_with_arrays()?;
    let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
    let stmts = crabgresql_parser::parse(sql)
        .map_err(|error| anyhow!("invalid SQL test fixture `{sql}`: {error}"))?;
    let ast::Statement::Query(query) = &stmts[0] else {
        bail!("unexpected statement in test fixture: {}", stmts[0]);
    };
    bind_query(&engine, &catalog, query)
        .map_err(anyhow::Error::new)
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

/// The parser hands a qualified subscript over as a root plus a chain of dots,
/// never as a `CompoundIdentifier`, so the binder resolves the path itself.
#[test]
fn a_qualified_subscript_binds_like_a_bare_one() -> anyhow::Result<()> {
    for sql in [
        "SELECT a[1] FROM arr",
        "SELECT arr.a[1] FROM arr",
        "SELECT x.a[1] FROM arr AS x",
    ] {
        let QueryPlan { projections, .. } = bind(sql)?.into_query()?;
        let BoundExpr::Subscript { base, ty, .. } = &projections[0] else {
            bail!("expected a Subscript for `{sql}`, got {:?}", projections[0]);
        };
        assert_eq!(*ty, PgType::Int4, "for `{sql}`");
        assert!(
            matches!(base.as_ref(), BoundExpr::ColumnRef { index: 1, .. }),
            "for `{sql}`, got {base:?}"
        );
    }
    Ok(())
}

/// The name is PG's strength 2, so a cast does not replace it with the type.
#[test]
fn a_qualified_subscript_is_named_after_the_column() -> anyhow::Result<()> {
    for sql in [
        "SELECT a[1] FROM arr",
        "SELECT arr.a[1] FROM arr",
        "SELECT a[1]::text FROM arr",
        "SELECT arr.a[1]::text FROM arr",
    ] {
        let names: Vec<String> = output_columns_of(&bind(sql)?)?
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, ["a"], "for `{sql}`");
    }
    Ok(())
}

#[test]
fn a_qualified_subscript_works_outside_the_target_list() -> anyhow::Result<()> {
    let QueryPlan { predicate, .. } =
        bind("SELECT id FROM arr WHERE arr.a[2] = 5")?.into_query()?;
    let Some(BoundExpr::Binary { left, .. }) = predicate.as_ref() else {
        bail!("expected a comparison predicate, got {predicate:?}");
    };
    assert!(
        matches!(left.as_ref(), BoundExpr::Subscript { .. }),
        "got {left:?}"
    );
    Ok(())
}

/// A chain of subscripts is one `Subscript` node holding one index per level —
/// arrays are flat, so `a[1][2]` is a single access, not two.
#[test]
fn a_subscript_chain_binds_to_one_node() -> anyhow::Result<()> {
    for sql in ["SELECT a[1][2] FROM arr", "SELECT arr.a[1][2] FROM arr"] {
        let QueryPlan { projections, .. } = bind(sql)?.into_query()?;
        let BoundExpr::Subscript { base, indexes, ty } = &projections[0] else {
            bail!("expected a Subscript for `{sql}`, got {:?}", projections[0]);
        };
        assert_eq!(*ty, PgType::Int4, "for `{sql}`");
        assert_eq!(indexes.len(), 2, "for `{sql}`");
        assert!(
            matches!(base.as_ref(), BoundExpr::ColumnRef { index: 1, .. }),
            "for `{sql}`, got {base:?}"
        );
    }
    Ok(())
}

/// A nested constructor keeps the element type of its innermost operands and the
/// *same* array type as them — PG has no nested array type, only more dimensions.
#[test]
fn a_nested_array_constructor_is_marked_nested() -> anyhow::Result<()> {
    for sql in [
        "SELECT ARRAY[[1,2],[3,4]] FROM arr",
        "SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] FROM arr",
    ] {
        let QueryPlan { projections, .. } = bind(sql)?.into_query()?;
        let BoundExpr::ArrayCtor {
            elem, nested, ty, ..
        } = &projections[0]
        else {
            bail!(
                "expected an ArrayCtor for `{sql}`, got {:?}",
                projections[0]
            );
        };
        assert!(nested, "for `{sql}`");
        assert_eq!(*elem, PgType::Int4, "for `{sql}`");
        assert_eq!(
            *ty,
            PgType::Array(crabgresql_types::oid::INT4),
            "for `{sql}`"
        );
    }
    let QueryPlan { projections, .. } = bind("SELECT ARRAY[1,2] FROM arr")?.into_query()?;
    assert!(
        matches!(&projections[0], BoundExpr::ArrayCtor { nested: false, .. }),
        "got {:?}",
        projections[0]
    );
    Ok(())
}

/// Only the shapes that need types this build has no representation for stay
/// `0A000` — slices, and field access on a composite.
#[test]
fn the_unsupported_subscript_shapes_keep_their_errors() -> anyhow::Result<()> {
    let slice = err("SELECT arr.a[1:2] FROM arr")?;
    assert_eq!(slice.code, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(slice.message, "array slice access is not supported yet");

    let field = err("SELECT (arr.a[1]).f FROM arr")?;
    assert_eq!(field.code, sqlstate::FEATURE_NOT_SUPPORTED);
    assert_eq!(
        field.message,
        "multi-dimensional or field subscripting is not supported yet"
    );
    Ok(())
}

#[test]
fn a_qualified_subscript_reports_the_base_it_could_not_resolve() -> anyhow::Result<()> {
    let missing = err("SELECT arr.nope[1] FROM arr")?;
    assert_eq!(missing.code, sqlstate::UNDEFINED_COLUMN);
    assert_eq!(missing.message, "column arr.nope does not exist");

    let not_an_array = err("SELECT arr.id[1] FROM arr")?;
    assert_eq!(not_an_array.code, sqlstate::DATATYPE_MISMATCH);
    assert_eq!(
        not_an_array.message,
        "cannot subscript type integer because it does not support subscripting"
    );
    Ok(())
}
