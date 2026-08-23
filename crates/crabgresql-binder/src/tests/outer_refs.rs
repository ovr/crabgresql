//! Correlated references, LATERAL and parameter substitution.

use super::common::*;

/// A derived table whose *body* correlates one of its own subqueries to its
/// own relations is ordinary SQL, not a LATERAL: nothing in it reaches the
/// enclosing query. `plan_has_outer_refs` used to answer "contains an
/// `OuterColumnRef` anywhere" rather than "contains one that escapes", so
/// the `debug_assert!` in `bind_from_item`'s Derived arm fired on it and
/// debug builds panicked — on `subselect`, `join` and `with` among others.
#[test]
fn a_derived_table_may_correlate_a_subquery_to_its_own_relations() -> anyhow::Result<()> {
    let plan = bound(
        "SELECT * FROM (SELECT id, (SELECT max(u.big) FROM t u WHERE u.id = s.id) AS m \
         FROM t s) d",
    )?;
    assert!(
        !plan_has_outer_refs(&plan),
        "the correlation is internal to the derived table, so nothing escapes"
    );
    Ok(())
}

/// The other half of the same predicate: a genuinely correlated subplan
/// must still report `true`. Guards against fixing the false positive by
/// making the comparison too strict.
#[test]
fn a_reference_to_the_immediate_parent_still_counts_as_escaping() -> anyhow::Result<()> {
    let plan = first_subplan("SELECT id, (SELECT max(u.big) FROM t u WHERE u.id = t.id) FROM t")?;
    assert!(plan_has_outer_refs(&plan));
    Ok(())
}

/// A reference that skips a level — bound in the innermost query but naming
/// the outermost one — escapes both of the queries it passes through, so it
/// is still visible from the middle subplan.
#[test]
fn a_reference_two_levels_out_escapes_the_intervening_query() -> anyhow::Result<()> {
    let plan = first_subplan(
        "SELECT id FROM t WHERE EXISTS ( \
           SELECT 1 FROM t u WHERE EXISTS (SELECT 1 FROM t v WHERE v.id = t.id))",
    )?;
    assert!(
        plan_has_outer_refs(&plan),
        "the inner `t.id` names the top query, two levels out"
    );
    Ok(())
}

/// A grouped query may carry a *non*-correlated subquery in its target list
/// even when that subquery nests a correlated one of its own — the indices
/// that `rewrite_over_aggregate` cannot line up are only the ones bound
/// against the aggregating query. The depth-blind predicate rejected this
/// with a spurious `0A000` in release builds too.
#[test]
fn a_self_contained_subquery_over_an_aggregate_is_not_rejected() -> anyhow::Result<()> {
    let _ = bound(
        "SELECT count(*), (SELECT max(u.big) FROM t u \
         WHERE EXISTS (SELECT 1 FROM t v WHERE v.id = u.id)) FROM t",
    )?;
    Ok(())
}

/// An outer reference in a *table function's arguments* escapes its subplan
/// like any other, so the marker above it is executed per outer row (with
/// `substitute_outer` filling the arguments) rather than folded once.
#[test]
fn an_outer_reference_in_a_table_fn_argument_escapes() -> anyhow::Result<()> {
    let plan = bound("SELECT id, ARRAY(SELECT g FROM generate_series(1, t.id) g) FROM t")?;
    let LogicalPlan::Query(QueryPlan { projections, .. }) = &plan else {
        bail!("expected a Query plan");
    };
    let Some(BoundExpr::ArraySubquery { subplan, .. }) = projections.get(1) else {
        bail!("expected an ARRAY subquery in the target list");
    };
    assert!(plan_has_outer_refs(&subplan.plan));
    Ok(())
}

/// A `LATERAL` item resolves the FROM items to its left, and the leaf that
/// carries it says so — that flag is what every correlation-depth walk keys on.
#[test]
fn lateral_marks_the_leaf_and_binds_the_sibling_at_level_one() -> anyhow::Result<()> {
    let plan = bound("SELECT * FROM t, LATERAL (SELECT t.id) x")?;
    let LogicalPlan::Join(JoinPlan { source, .. }) = &plan else {
        bail!("expected a Join plan");
    };
    let JoinExpr::Join { right, .. } = source else {
        bail!("expected a binary join at the root");
    };
    let JoinExpr::Input {
        input: JoinInput::Subplan(body),
        lateral,
        ..
    } = right.as_ref()
    else {
        bail!("expected a subplan leaf on the right");
    };
    assert!(lateral, "the body references `t`, so the leaf is lateral");
    assert!(
        plan_has_outer_refs(body),
        "`t.id` is an outer reference from the body's point of view"
    );
    // And the reference is *not* one the enclosing statement fills: the lateral
    // join node above it does, one level in.
    assert!(!plan_has_outer_refs(&plan));
    Ok(())
}

/// `LATERAL` on a leftmost FROM item is legal and inert — there is nothing to
/// its left. The leaf must not be marked, or the executor would rebuild it per
/// row of a left input that does not exist.
#[test]
fn a_leftmost_lateral_item_is_not_marked() -> anyhow::Result<()> {
    let plan = bound("SELECT * FROM LATERAL (SELECT 1) x")?;
    assert!(!plan_has_outer_refs(&plan));
    Ok(())
}

/// A `LATERAL` item that references only an *enclosing* query keeps its
/// references at level 1: the level pushed for the (unused) siblings is dropped
/// again, so the enclosing boundary still fills them.
#[test]
fn an_unused_lateral_level_is_dropped_again() -> anyhow::Result<()> {
    let subplan =
        first_subplan("SELECT id, (SELECT x.c FROM t u, LATERAL (SELECT t.id AS c) x) FROM t")?;
    let LogicalPlan::Join(JoinPlan { source, .. }) = &subplan else {
        bail!("expected a Join plan");
    };
    let JoinExpr::Join { right, .. } = source else {
        bail!("expected a binary join at the root");
    };
    let JoinExpr::Input { lateral, .. } = right.as_ref() else {
        bail!("expected a leaf on the right");
    };
    assert!(
        !lateral,
        "`t` is the enclosing query, not the sibling `u`, so nothing is lateral"
    );
    assert!(
        plan_has_outer_refs(&subplan),
        "the reference escapes to the enclosing query, at level 1"
    );
    Ok(())
}

/// A plain subquery in FROM may not see its siblings, and PostgreSQL says so
/// precisely — naming the entry and the keyword that would reach it — rather
/// than reporting a missing FROM-clause entry.
#[test]
fn a_non_lateral_from_subquery_is_refused_by_name() -> anyhow::Result<()> {
    let error = bind_err("SELECT * FROM t, (SELECT t.id) x")?;
    assert_eq!(error.code, sqlstate::UNDEFINED_TABLE);
    assert_eq!(
        error.message,
        "invalid reference to FROM-clause entry for table \"t\""
    );
    assert_eq!(
        error.detail.as_deref(),
        Some(
            "There is an entry for table \"t\", but it cannot be referenced from this part of \
             the query."
        )
    );
    assert_eq!(
        error.hint.as_deref(),
        Some("To reference that table, you must mark this subquery with LATERAL.")
    );

    // The unqualified form blames the column instead, again as PG does.
    let error = bind_err("SELECT * FROM t, (SELECT id) x")?;
    assert_eq!(error.code, sqlstate::UNDEFINED_COLUMN);
    assert_eq!(error.message, "column \"id\" does not exist");
    assert_eq!(
        error.detail.as_deref(),
        Some(
            "There is a column named \"id\" in table \"t\", but it cannot be referenced from \
             this part of the query."
        )
    );
    Ok(())
}

/// A plain FROM subquery *is* correlated to the enclosing queries, though —
/// only its siblings are off limits. This is the case the barrier must let
/// through, and it stays at the same correlation depth as the item itself.
#[test]
fn a_non_lateral_from_subquery_may_reference_an_enclosing_query() -> anyhow::Result<()> {
    let subplan = first_subplan("SELECT id, (SELECT x.c FROM t u, (SELECT t.id AS c) x) FROM t")?;
    assert!(plan_has_outer_refs(&subplan));
    Ok(())
}

/// A `LATERAL` reference across a RIGHT or FULL join is refused: the left row it
/// would read is not guaranteed to exist. PG's own wording, DETAIL included.
#[test]
fn lateral_across_a_right_or_full_join_is_refused() -> anyhow::Result<()> {
    for sql in [
        "SELECT * FROM t RIGHT JOIN LATERAL (SELECT t.id) x ON true",
        "SELECT * FROM t FULL JOIN LATERAL (SELECT t.id) x ON true",
    ] {
        let error = bind_err(sql)?;
        assert_eq!(error.code, sqlstate::UNDEFINED_TABLE, "for `{sql}`");
        assert_eq!(
            error.message, "invalid reference to FROM-clause entry for table \"t\"",
            "for `{sql}`"
        );
        assert_eq!(
            error.detail.as_deref(),
            Some("The combining JOIN type must be INNER or LEFT for a LATERAL reference."),
            "for `{sql}`"
        );
    }

    // A function FROM item is implicitly lateral, so it is refused the same way
    // without the keyword ever being written.
    let error = bind_err("SELECT * FROM t RIGHT JOIN generate_series(1, t.id) g ON true")?;
    assert_eq!(
        error.detail.as_deref(),
        Some("The combining JOIN type must be INNER or LEFT for a LATERAL reference.")
    );

    // A *plain* subquery there is out of reach too, but `LATERAL` would not
    // reach it either — so PG states the fact and offers no hint.
    let error = bind_err("SELECT * FROM t RIGHT JOIN (SELECT t.id) x ON true")?;
    assert_eq!(
        error.detail.as_deref(),
        Some(
            "There is an entry for table \"t\", but it cannot be referenced from this part of \
             the query."
        )
    );
    assert_eq!(
        error.hint, None,
        "LATERAL would not help across a RIGHT JOIN"
    );
    Ok(())
}

/// A `LATERAL` body across a RIGHT join may still reference an *enclosing*
/// query — only the left row is off limits. No level is pushed there, so its
/// level-1 reference has to stay one the enclosing boundary fills; treating it
/// as lateral would read it out of a left row that never existed.
#[test]
fn a_refused_lateral_still_reaches_the_enclosing_query() -> anyhow::Result<()> {
    let subplan = first_subplan(
        "SELECT (SELECT x.c FROM t u RIGHT JOIN LATERAL (SELECT o.id AS c) x ON true) FROM t o",
    )?;
    let LogicalPlan::Join(JoinPlan { source, .. }) = &subplan else {
        bail!("expected a Join plan");
    };
    let JoinExpr::Join { right, .. } = source else {
        bail!("expected a binary join at the root");
    };
    let JoinExpr::Input { lateral, .. } = right.as_ref() else {
        bail!("expected a leaf on the right");
    };
    assert!(!lateral, "no level was pushed, so nothing became lateral");
    assert!(
        plan_has_outer_refs(&subplan),
        "`o.id` still escapes to the enclosing query"
    );
    Ok(())
}

/// The shapes PostgreSQL answers and this build does not: a lateral item
/// *anywhere in a join chain* reaching back into an earlier comma group, whose
/// columns no node of that chain is fed. Refused by name rather than resolved
/// outward, which would silently answer a different query.
///
/// The chain-*leading* cases matter twice over. Binding them as lateral leaves a
/// leaf that is the bottom-left of the chain, with the cross join to the earlier
/// groups spliced in above it — so its `level: 1` references have no join node
/// to fill them, and the plan either trips the planner's `debug_assert` or
/// reaches execution with an unsubstituted reference.
#[test]
fn lateral_into_another_comma_group_is_an_honest_gap() -> anyhow::Result<()> {
    for sql in [
        // Inside the chain.
        "SELECT * FROM t, t u JOIN LATERAL (SELECT t.id) x ON true",
        // Leading the chain, as a subquery and as an implicitly-lateral table
        // function, over each of the join shapes that put a factor to its right.
        "SELECT * FROM t, LATERAL (SELECT t.id AS z) x CROSS JOIN t u",
        "SELECT * FROM t, LATERAL (SELECT t.id AS z) x JOIN t u ON true",
        "SELECT * FROM t, LATERAL (SELECT t.id AS z) x RIGHT JOIN t u ON true",
        "SELECT * FROM t, generate_series(1, t.id) g JOIN t u ON true",
    ] {
        let error = bind_err(sql)?;
        assert_eq!(error.code, "0A000", "for `{sql}`");
        assert_eq!(
            error.message,
            "LATERAL reference to \"t\" from another comma-separated FROM item is not \
             supported yet",
            "for `{sql}`"
        );
    }

    // The same item without a chain after it is answered as usual — the gap is
    // the *position*, not the reference.
    let plan = bound("SELECT * FROM t, LATERAL (SELECT t.id AS z) x")?;
    assert!(!plan_has_outer_refs(&plan));
    Ok(())
}

/// The subplan of the first expression-subquery marker in `sql`'s target
/// list, or failing that in its `WHERE`, ready for [`substitute_outer`].
fn first_subplan(sql: &str) -> anyhow::Result<LogicalPlan> {
    fn find(exprs: &[BoundExpr]) -> Option<LogicalPlan> {
        exprs.iter().find_map(|e| match e {
            BoundExpr::ScalarSubquery { subplan, .. } | BoundExpr::Exists { subplan, .. } => {
                Some((*subplan.plan).clone())
            }
            _ => None,
        })
    }
    let plan = bound(sql)?;
    let (LogicalPlan::Query(QueryPlan {
        projections,
        predicate,
        ..
    })
    | LogicalPlan::Subquery(SubqueryPlan {
        projections,
        predicate,
        ..
    })) = &plan
    else {
        bail!(
            "expected a Query or Subquery for `{sql}`, got {}",
            plan_name(&plan)
        );
    };
    find(projections)
        .or_else(|| {
            predicate
                .as_ref()
                .and_then(|p| find(std::slice::from_ref(p)))
        })
        .with_context(|| format!("no expression subquery in `{sql}`"))
}

/// `substitute_outer` must increment its depth exactly where the binder
/// pushed a correlation level — only at an expression-subquery marker. The
/// window chain's wrapping `Subquery` is *not* a level, so a correlated
/// reference below it is substituted at depth 1 like any other.
///
/// Before the fix this left the reference stranded, which surfaced as an
/// internal "was not substituted" error at execution.
#[test]
fn a_correlated_reference_below_a_window_chain_is_substituted() -> anyhow::Result<()> {
    let mut plan = first_subplan("SELECT id, (SELECT sum(big + t.id) OVER () FROM t u) FROM t")?;
    assert!(plan_has_outer_refs(&plan), "the subplan starts correlated");
    substitute_outer(&mut plan, &[Value::Int4(7)]);
    assert!(
        !plan_has_outer_refs(&plan),
        "the window chain's wrapper is not a query nesting level"
    );
    Ok(())
}

/// The same depth bug, reached without any window at all: `attach_sort`
/// wraps a sorted `LIMIT` in a synthetic `Subquery` too. Pre-existing, fixed
/// by the same change.
#[test]
fn a_correlated_reference_below_a_sorted_limit_is_substituted() -> anyhow::Result<()> {
    let mut plan = first_subplan(
        "SELECT id FROM t WHERE EXISTS ( (SELECT 1 FROM t u WHERE u.id = t.id LIMIT 1) \
         ORDER BY 1 )",
    )?;
    assert!(plan_has_outer_refs(&plan), "the subplan starts correlated");
    substitute_outer(&mut plan, &[Value::Int4(7)]);
    assert!(!plan_has_outer_refs(&plan));
    Ok(())
}

/// A `$n` can appear in an argument or in the OVER clause itself, and both
/// have to be substituted — the window node's expressions are not reached by
/// the projection walk.
#[test]
fn substitute_params_reaches_into_a_window_spec() -> anyhow::Result<()> {
    let engine = engine_with_table()?;
    let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
    // Declared, as an extended-protocol Parse would: a bare `PARTITION BY $1`
    // gives the binder nothing to infer from, and PG rejects it too.
    let params = param_ctx_extended(vec![Some(PgType::Int4)]);
    let stmts =
        crabgresql_parser::parse("SELECT rank() OVER (PARTITION BY $1 ORDER BY name) FROM t")?;
    let ast::Statement::Query(query) = &stmts[0] else {
        bail!("expected a query");
    };
    let mut plan = bind_query_with_params(&engine, &catalog, query, &params)?;
    substitute_params(&mut plan, &[Value::Int4(7)]);
    let SubqueryPlan { source, .. } = plan.into_subquery()?;
    let WindowPlan { spec, .. } = source.into_window()?;
    assert_eq!(
        spec.partition_by,
        vec![BoundExpr::Const {
            value: Value::Int4(7),
            ty: PgType::Int4
        }]
    );
    Ok(())
}
