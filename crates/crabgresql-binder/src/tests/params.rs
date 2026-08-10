//! Placeholders: declared types, inference and `pg_typeof`.

use super::common::*;

// --- bind-parameter ($1, $2, …) inference -------------------------------

/// Bind `sql` for the extended protocol with the given declared parameter
/// types, returning both the plan result and the shared context (so tests
/// can read back the inferred types).
fn bind_params(
    sql: &str,
    declared: Vec<Option<PgType>>,
) -> anyhow::Result<(Result<LogicalPlan, BindError>, ParamCtx)> {
    let engine = engine_with_table()?;
    let catalog: Arc<dyn TypeCatalog> = Arc::new(crabgresql_storage_api::EmptyTypeCatalog);
    let ctx = param_ctx_extended(declared);
    let stmts = crabgresql_parser::parse(sql)
        .map_err(|error| anyhow!("invalid SQL test fixture `{sql}`: {error}"))?;
    let plan = match &stmts[0] {
        ast::Statement::Query(q) => bind_query_with_params(&engine, &catalog, q, &ctx),
        other => bail!("unexpected statement: {other}"),
    };
    Ok((plan, ctx))
}

#[test]
fn declared_param_binds_and_reports_its_type() -> anyhow::Result<()> {
    // A client-declared int4 `$1` binds directly to a Param node.
    let (plan, ctx) = bind_params("SELECT $1", vec![Some(PgType::Int4)])?;
    let plan = plan.context("declared $1 binds")?;
    assert_eq!(param_types(&ctx), vec![Some(PgType::Int4)]);
    let ValuesPlan { rows, .. } = plan.into_values()?;
    assert_eq!(
        rows[0][0],
        BoundExpr::Param {
            index: 0,
            ty: PgType::Int4
        }
    );
    Ok(())
}

#[test]
fn undeclared_param_infers_type_from_comparison() -> anyhow::Result<()> {
    // `$1 = big` against `big int8` deduces $1 as int8.
    let (plan, ctx) = bind_params("SELECT $1 = big FROM t", vec![])?;
    plan.context("$1 = big binds")?;
    assert_eq!(param_types(&ctx), vec![Some(PgType::Int8)]);
    Ok(())
}

#[test]
fn undeclared_param_infers_type_from_cast() -> anyhow::Result<()> {
    let (plan, ctx) = bind_params("SELECT $1::int4", vec![])?;
    plan.context("$1::int4 binds")?;
    assert_eq!(param_types(&ctx), vec![Some(PgType::Int4)]);
    Ok(())
}

#[test]
fn param_reused_across_sites_unifies() -> anyhow::Result<()> {
    // The same `$1` appears twice; both sites agree on int8.
    let (plan, ctx) = bind_params("SELECT $1 = big, $1 = big FROM t", vec![])?;
    plan.context("repeated $1 binds")?;
    assert_eq!(param_types(&ctx), vec![Some(PgType::Int8)]);
    Ok(())
}

/// The bind error for a param query.
fn param_err(sql: &str, declared: Vec<Option<PgType>>) -> anyhow::Result<BindError> {
    match bind_params(sql, declared)?.0 {
        Err(e) => Ok(e),
        Ok(_) => bail!("expected a bind error for: {sql}"),
    }
}

#[test]
fn undetermined_param_is_42p18() -> anyhow::Result<()> {
    // A bare `$1` with no context and no declaration cannot be typed.
    let err = param_err("SELECT $1", vec![])?;
    assert_eq!(err.code, "42P18");
    assert_eq!(err.message, "could not determine data type of parameter $1");
    Ok(())
}

/// `pg_typeof` reads its argument's type without evaluating it, so it gives
/// a bare `$n` no context to be typed from and has to give up on the spot.
#[test]
fn pg_typeof_of_an_untyped_param_is_42p18() -> anyhow::Result<()> {
    let err = param_err("SELECT pg_typeof($1)", vec![])?;
    assert_eq!(err.code, "42P18");
    assert_eq!(err.message, "could not determine data type of parameter $1");
    // A declared parameter has a type to report, so it binds.
    assert!(
        bind_params("SELECT pg_typeof($1)", vec![Some(PgType::Int4)])?
            .0
            .is_ok()
    );
    Ok(())
}

/// The reported OID rides on the `ScalarFn`, and the argument *stays* in
/// `args`. Keeping it is what makes the argument still get evaluated and still
/// be seen by every pass that walks a call's arguments — aggregate extraction,
/// GROUP BY validation, volatility, deparse. An earlier version collapsed the
/// whole call to a bare OID constant and broke all four.
#[test]
fn pg_typeof_reports_the_type_and_keeps_the_argument() -> anyhow::Result<()> {
    use crate::ScalarFn;
    use crabgresql_types::{RegKind, oid};
    let call_of = |sql: &str| -> anyhow::Result<(u32, BoundExpr)> {
        let ValuesPlan { rows, .. } = bound_values(sql)?;
        let BoundExpr::FuncCall { func, ret, args } = &rows[0][0] else {
            bail!("expected a FuncCall for: {sql}");
        };
        assert_eq!(*ret, PgType::Reg(RegKind::Type));
        let ScalarFn::PgTypeof(reported) = *func else {
            bail!("expected ScalarFn::PgTypeof for: {sql}");
        };
        assert_eq!(args.len(), 1, "the argument must survive: {sql}");
        Ok((reported, args[0].clone()))
    };
    let oid_of = |sql: &str| -> anyhow::Result<u32> { Ok(call_of(sql)?.0) };

    assert_eq!(oid_of("SELECT pg_typeof(1)")?, PgType::Int4.oid());
    assert_eq!(
        oid_of("SELECT pg_typeof('2020-01-01'::timestamptz)")?,
        PgType::TimestampTz.oid()
    );
    // The typmod is not part of the OID, so it cannot be reported.
    assert_eq!(
        oid_of("SELECT pg_typeof(1::numeric(10,2))")?,
        PgType::Numeric.oid()
    );
    // An untyped literal is genuinely `unknown`, not text.
    assert_eq!(oid_of("SELECT pg_typeof('abc')")?, oid::UNKNOWN);
    assert_eq!(oid_of("SELECT pg_typeof(NULL)")?, oid::UNKNOWN);

    // The kept argument is the user's own expression, not a folded OID.
    let (reported, arg) = call_of("SELECT pg_typeof(1 + 1)")?;
    assert_eq!(reported, PgType::Int4.oid());
    assert!(
        matches!(arg, BoundExpr::Binary { .. }),
        "the `1 + 1` expression should still be there, got {arg:?}"
    );

    // An aggregate inside the argument is still visible to the extraction
    // pass, so the query groups instead of scanning. `agg_of` errors out if
    // the plan is not an Aggregate, which is exactly the regression to catch.
    let (_, aggregates, _, _) = agg_of("SELECT pg_typeof(count(*)) FROM t")?;
    assert_eq!(aggregates.len(), 1);
    // ... and a bare column beside a grouped one is still the GROUP BY error.
    let err = bind_err("SELECT pg_typeof(id) FROM t GROUP BY name")?;
    assert_eq!(err.code, "42803");
    Ok(())
}

/// Any arity but one falls through to ordinary resolution, so a user-defined
/// overload of this name stays reachable. With no such routine the
/// fall-through still reports PG's `42883`.
#[test]
fn pg_typeof_is_only_unary() -> anyhow::Result<()> {
    let err = bind_err("SELECT pg_typeof(1, 2)")?;
    assert_eq!(err.code, "42883");
    assert_eq!(
        err.message,
        "function pg_typeof(integer, integer) does not exist"
    );
    Ok(())
}

#[test]
fn conflicting_param_deductions_are_42p18() -> anyhow::Result<()> {
    // `$1 IN (big, name)` clones the still-untyped `$1` for each comparison,
    // so one arm deduces int8 and the other text before either is fixed —
    // an inconsistency PG reports as 42P18.
    let err = param_err("SELECT $1 IN (big, name) FROM t", vec![])?;
    assert_eq!(err.code, "42P18");
    assert_eq!(err.message, "inconsistent types deduced for parameter $1");
    Ok(())
}

#[test]
fn param_in_simple_query_is_42p02() -> anyhow::Result<()> {
    // The simple-query entry point forbids parameters entirely.
    let err = bind_err("SELECT $1")?;
    assert_eq!(err.code, "42P02");
    assert_eq!(err.message, "there is no parameter $1");
    Ok(())
}
