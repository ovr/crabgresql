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

/// A FROM subquery is bound with an empty outer scope, so a parsed LATERAL
/// cannot mean what it says. Report the missing feature rather than the
/// `42703` that falls out of binding it as a plain derived table.
///
/// TODO: bind a LATERAL FROM item against the scope of the FROM items to its
/// left, so the correlation resolves instead of being rejected.
#[test]
fn lateral_is_reported_as_unsupported() -> anyhow::Result<()> {
    let error = bind_err("SELECT * FROM t, LATERAL (SELECT t.id) x")?;
    assert_eq!(error.code, "0A000");
    assert_eq!(error.message, "LATERAL is not supported yet");
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
