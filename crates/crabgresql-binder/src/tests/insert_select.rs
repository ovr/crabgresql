//! `INSERT ... SELECT`, `TABLE`, DEFAULT and RETURNING.

use super::common::*;

#[test]
fn insert_select_binds_as_query_source() -> anyhow::Result<()> {
    // A SELECT source produces a query-source Insert whose projection list is
    // full-width in schema order (unlisted columns take their defaults).
    let LogicalPlan::Insert(InsertPlan {
        source: InsertSource::Query { projections, .. },
        ..
    }) = bind_one("INSERT INTO t (id, name) SELECT id, name FROM t")?
    else {
        panic!("expected a query-source Insert");
    };
    assert_eq!(projections.len(), 4);
    // The two listed columns reference the source row by position.
    assert!(matches!(
        projections[0],
        BoundExpr::ColumnRef { index: 0, .. }
    ));
    assert!(matches!(
        projections[2],
        BoundExpr::ColumnRef { index: 1, .. }
    ));
    Ok(())
}

#[test]
fn insert_select_arity_mismatches_match_pg() {
    let too_many = bind_err("INSERT INTO t (id) SELECT id, name FROM t");
    assert_eq!(
        too_many.message,
        "INSERT has more expressions than target columns"
    );
    let too_few = bind_err("INSERT INTO t (id, name) SELECT id FROM t");
    assert_eq!(
        too_few.message,
        "INSERT has more target columns than expressions"
    );
}

#[test]
fn insert_select_type_mismatch_reports_datatype_mismatch() {
    // int4 (id) does not assign to a bool column.
    let e = bind_err("INSERT INTO t (flag) SELECT id FROM t");
    assert_eq!(e.code, sqlstate::DATATYPE_MISMATCH);
}

#[test]
fn insert_table_source_binds_as_query() -> anyhow::Result<()> {
    // `INSERT ... TABLE t` is `INSERT ... SELECT * FROM t`.
    let LogicalPlan::Insert(InsertPlan {
        source: InsertSource::Query { projections, .. },
        ..
    }) = bind_one("INSERT INTO t TABLE t")?
    else {
        panic!("expected a query-source Insert");
    };
    assert_eq!(projections.len(), 4);
    Ok(())
}

#[test]
fn table_statement_binds_select_star() {
    let QueryPlan { columns, .. } = bound_query("TABLE t");
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "big", "name", "flag"]);
}

#[test]
fn table_preserves_quoted_identifier_case() {
    // A case-sensitive relation reached via `TABLE "MixedCase"` must keep its
    // quoting, exactly as `SELECT * FROM "MixedCase"` does; an unquoted name
    // folds to lower case and does not resolve (matching PostgreSQL).
    let engine = crabgresql_pg_engine::ephemeral_engine();
    if let Err(error) = engine.create_table(TableSchema::new(
        "MixedCase",
        vec![Column::new("id", PgType::Int4)],
    )) {
        panic!("failed to create test table: {error}");
    }
    let engine: Arc<dyn TableEngine> = engine;
    let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
    let bind = |sql: &str| -> Result<LogicalPlan, BindError> {
        let stmts = crabgresql_parser::parse(sql).expect("valid SQL");
        match &stmts[0] {
            ast::Statement::Query(q) => bind_query(&engine, &catalog, q),
            ast::Statement::Insert(i) => bind_insert(&engine, &catalog, i),
            other => panic!("unexpected statement: {other}"),
        }
    };

    // Quoted keeps case → resolves, as a statement and as an INSERT source.
    assert!(bind("TABLE \"MixedCase\"").is_ok());
    assert!(bind("INSERT INTO \"MixedCase\" TABLE \"MixedCase\"").is_ok());
    // Unquoted folds to `mixedcase`, which does not exist.
    match bind("TABLE MixedCase") {
        Err(e) => assert_eq!(e.code, sqlstate::UNDEFINED_TABLE),
        Ok(_) => panic!("unquoted MixedCase must not resolve to \"MixedCase\""),
    }
}

#[test]
fn insert_source_query_clauses_are_executed_not_rejected() -> anyhow::Result<()> {
    // A VALUES source carrying ORDER BY / LIMIT is a full query in PG: it must
    // be executed as one (a query source), not silently dropped or rejected.
    for sql in [
        "INSERT INTO t (id) VALUES (1), (2) LIMIT 1",
        "INSERT INTO t (id) VALUES (1), (2) ORDER BY 1",
    ] {
        let LogicalPlan::Insert(InsertPlan {
            source: InsertSource::Query { .. },
            ..
        }) = bind_one(sql)?
        else {
            panic!("expected a query-source Insert for: {sql}");
        };
    }
    Ok(())
}

#[test]
fn default_keyword_binds_as_typed_null_without_a_declared_default() -> anyhow::Result<()> {
    let LogicalPlan::Insert(InsertPlan {
        source: InsertSource::Values(rows),
        ..
    }) = bind_one("INSERT INTO t (id) VALUES (DEFAULT)")?
    else {
        panic!("expected Insert with a VALUES source");
    };
    assert_eq!(
        rows[0][0],
        BoundExpr::Const {
            value: Value::Null,
            ty: PgType::Int4,
        }
    );
    assert!(bind_one("UPDATE t SET id = DEFAULT").is_ok());

    Ok(())
}

#[test]
fn returning_binds_output_columns_for_each_dml() -> anyhow::Result<()> {
    // INSERT: `*` expands the whole table, a computed column carries an alias.
    let insert = bound("INSERT INTO t (id) VALUES (1) RETURNING *, id + 1 AS next");
    let LogicalPlan::Insert(InsertPlan {
        returning: Some(_), ..
    }) = &insert
    else {
        panic!("expected Insert with RETURNING");
    };
    let cols = output_columns_of(&insert)?;
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["id", "big", "name", "flag", "next"]);
    assert_eq!(
        cols.last()
            .expect("RETURNING produced at least one column")
            .ty,
        PgType::Int4
    );

    // UPDATE and DELETE report their RETURNING columns too (used by Describe).
    let update = bound("UPDATE t SET id = 1 RETURNING id, name");
    assert!(matches!(
        update,
        LogicalPlan::Update(UpdatePlan {
            returning: Some(_),
            ..
        })
    ));
    let names: Vec<String> = output_columns_of(&update)?
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, vec!["id", "name"]);

    let delete = bound("DELETE FROM t RETURNING name, id");
    let names: Vec<String> = output_columns_of(&delete)?
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, vec!["name", "id"]);

    // A RETURNING expression over an unknown column still errors.
    assert_eq!(bind_err("DELETE FROM t RETURNING nope").code, "42703");

    Ok(())
}

#[test]
fn returning_rejects_aggregates_and_set_returning_functions() {
    // PostgreSQL rejects both at bind time (no aggregate/ProjectSet node
    // exists above a data-modifying statement to consume them).
    let agg = bind_err("UPDATE t SET id = 1 RETURNING count(*)");
    assert_eq!(agg.code, "42803");
    assert_eq!(
        agg.message,
        "aggregate functions are not allowed in RETURNING"
    );
    assert_eq!(bind_err("DELETE FROM t RETURNING max(id)").code, "42803");

    let srf = bind_err("INSERT INTO t (id) VALUES (1) RETURNING generate_series(1, id)");
    assert_eq!(srf.code, "0A000");
    assert_eq!(
        srf.message,
        "set-returning functions are not allowed in RETURNING"
    );
}

#[test]
fn ragged_values_lists_are_42601() {
    let e = bind_err("INSERT INTO t VALUES (1, 2), (3)");
    assert_eq!(e.code, "42601");
    assert_eq!(e.message, "VALUES lists must all be the same length");
}
