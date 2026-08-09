//! INSERT, UPDATE and DELETE binding.

use super::common::*;

#[test]
fn insert_coerces_cells_to_column_types() -> anyhow::Result<()> {
    let LogicalPlan::Insert(InsertPlan {
        source: InsertSource::Values(rows),
        ..
    }) = bind_one("INSERT INTO t (id, name) VALUES ('7', 'x')")?
    else {
        panic!("expected Insert with a VALUES source");
    };
    // Full-width row in schema order, missing columns padded with NULL.
    assert_eq!(rows[0].len(), 4);
    assert_eq!(
        rows[0][0],
        BoundExpr::Const {
            value: Value::Int4(7),
            ty: PgType::Int4
        }
    );
    assert_eq!(
        rows[0][1],
        BoundExpr::Const {
            value: Value::Null,
            ty: PgType::Int8
        }
    );

    Ok(())
}

#[test]
fn insert_type_mismatch_is_42804_with_column_context() {
    let e = bind_err("INSERT INTO t (flag) VALUES (1)");
    assert_eq!(e.code, "42804");
    assert_eq!(
        e.message,
        "column \"flag\" is of type boolean but expression is of type integer"
    );
}

#[test]
fn insert_column_refs_in_values_are_undefined() {
    let e = bind_err("INSERT INTO t (id) VALUES (id)");
    assert_eq!(e.code, "42703");
}

#[test]
fn update_binds_assignments_by_index() -> anyhow::Result<()> {
    let UpdatePlan {
        assignments,
        predicate,
        ..
    } = bind_one("UPDATE t SET name = 'x', id = id + 1 WHERE flag")?.expect_update();
    assert_eq!(assignments.len(), 2);
    assert_eq!(assignments[0].0, 2);
    assert_eq!(assignments[1].0, 0);
    assert!(predicate.is_some());

    Ok(())
}

#[test]
fn update_duplicate_assignment_is_42601() {
    let e = bind_err("UPDATE t SET id = 1, id = 2");
    assert_eq!(e.code, "42601");
    assert_eq!(e.message, "multiple assignments to same column \"id\"");
}

#[test]
fn update_unknown_column_names_the_relation() {
    let e = bind_err("UPDATE t SET nope = 1");
    assert_eq!(e.code, "42703");
    assert_eq!(
        e.message,
        "column \"nope\" of relation \"t\" does not exist"
    );
}

#[test]
fn update_assignment_coerces_to_column_type() -> anyhow::Result<()> {
    let UpdatePlan { assignments, .. } = bind_one("UPDATE t SET id = big")?.expect_update();
    assert_eq!(
        assignments[0].1,
        BoundExpr::Coerce {
            expr: Box::new(BoundExpr::ColumnRef {
                index: 1,
                ty: PgType::Int8
            }),
            ty: PgType::Int4,
        }
    );

    Ok(())
}

#[test]
fn delete_binds_predicate() -> anyhow::Result<()> {
    let DeletePlan { predicate, .. } = bind_one("DELETE FROM t WHERE id = 1")?.expect_delete();
    assert!(predicate.is_some());
    let DeletePlan { predicate, .. } = bind_one("DELETE FROM t")?.expect_delete();
    assert!(predicate.is_none());

    Ok(())
}

#[test]
fn unsupported_forms_stay_0a000() {
    for sql in [
        "UPDATE t SET (id, name) = (1, 'x')",
        "DELETE FROM t USING t AS u",
    ] {
        let e = bind_err(sql);
        assert_eq!(e.code, "0A000", "for: {sql}");
    }
}
