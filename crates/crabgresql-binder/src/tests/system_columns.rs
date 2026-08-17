//! Which relations a query demands each system column for.

use super::common::*;

/// The demand of a SELECT for `col`, as `(bare, sorted qualifiers)`.
fn demand_of_col(sql: &str, col: SysCol) -> anyhow::Result<(bool, Vec<String>)> {
    let stmts = crabgresql_parser::parse(sql)
        .map_err(|error| anyhow!("invalid SQL test fixture `{sql}`: {error}"))?;
    let ast::Statement::Query(query) = &stmts[0] else {
        bail!("expected a query: {sql}");
    };
    let ast::SetExpr::Select(select) = query.body.as_ref() else {
        bail!("expected a plain SELECT: {sql}");
    };
    let demand = SystemDemand::of_select(select, &query.order_by);
    // A qualifier is only observable through `wants`, which is the interface the
    // binder itself uses; probe it with the qualifiers the fixtures can name.
    let mut qualified: Vec<String> = ["t", "u", "x", "T"]
        .into_iter()
        .filter(|q| !demand.is_bare(col) && demand.wants(q).contains(&col))
        .map(String::from)
        .collect();
    qualified.sort();
    Ok((demand.is_bare(col), qualified))
}

/// The demand of a SELECT for `tableoid` — the shape the original test used.
fn demand_of(sql: &str) -> anyhow::Result<(bool, Vec<String>)> {
    demand_of_col(sql, SysCol::TableOid)
}

#[test]
fn tableoid_demand_finds_every_expression_position() -> anyhow::Result<()> {
    // Nothing names it: the common case must widen nothing at all.
    assert_eq!(demand_of("SELECT id FROM t")?.0, false);
    assert!(demand_of("SELECT id FROM t")?.1.is_empty());

    // Every clause an expression can sit in, including a correlated
    // subquery, which is the position a structural walk is likeliest to
    // miss.
    for sql in [
        "SELECT tableoid FROM t",
        "SELECT id FROM t WHERE tableoid > 0",
        "SELECT id FROM t GROUP BY tableoid",
        "SELECT count(*) FROM t GROUP BY id HAVING max(tableoid) > 0",
        "SELECT id FROM t ORDER BY tableoid",
        "SELECT id FROM t JOIN t u ON u.id = tableoid",
        "SELECT (SELECT tableoid) FROM t",
        "SELECT CASE WHEN true THEN tableoid ELSE 0 END FROM t",
    ] {
        assert!(demand_of(sql)?.0, "unqualified reference missed in `{sql}`");
    }

    // Qualified references name the relation they belong to, and only it.
    assert_eq!(
        demand_of("SELECT t.tableoid FROM t, u")?,
        (false, vec!["t".to_string()])
    );
    assert_eq!(
        demand_of("SELECT id FROM t WHERE (SELECT x.tableoid FROM u x) = 1")?,
        (false, vec!["x".to_string()])
    );
    // A quoted qualifier keeps its case, exactly as `normalize_ident` would
    // leave it, so the two spellings match the same relation.
    assert_eq!(
        demand_of("SELECT \"T\".tableoid FROM t \"T\"")?,
        (false, vec!["T".to_string()])
    );
    // Schema-qualified: the relation is addressed by its own name.
    assert_eq!(
        demand_of("SELECT public.t.tableoid FROM public.t")?,
        (false, vec!["t".to_string()])
    );

    // A longer identifier merely containing the word is not a reference.
    for sql in [
        "SELECT mytableoid FROM t",
        "SELECT tableoid_x FROM t",
        "SELECT t.tableoids FROM t",
    ] {
        let (bare, qualified) = demand_of(sql)?;
        assert!(!bare && qualified.is_empty(), "false positive in `{sql}`");
    }

    // Documented over-approximation: a literal spelling the word costs an
    // unreferenced slot, never a wrong answer.
    assert!(demand_of("SELECT 'tableoid' FROM t")?.0);
    Ok(())
}

/// The five columns added after `tableoid` go through the same scanner, so what
/// needs pinning is that each is recognised **independently**: a query naming
/// `ctid` must not buy an `xmin` slot, and the qualifier must attach to the
/// column it precedes rather than to all of them.
#[test]
fn each_system_column_is_demanded_on_its_own() -> anyhow::Result<()> {
    for col in SysCol::ALL {
        let sql = format!("SELECT {} FROM t", col.name());
        for other in SysCol::ALL {
            let (bare, _) = demand_of_col(&sql, other)?;
            assert_eq!(
                bare,
                other == col,
                "`{sql}` must demand {} and nothing else, but {} came back {bare}",
                col.name(),
                other.name(),
            );
        }
    }

    // Qualifiers attach per column: `t.ctid` and a bare `xmin` in one query.
    let sql = "SELECT t.ctid, xmin FROM t, u";
    assert_eq!(
        demand_of_col(sql, SysCol::Ctid)?,
        (false, vec!["t".to_string()])
    );
    assert!(demand_of_col(sql, SysCol::Xmin)?.0);
    assert!(!demand_of_col(sql, SysCol::Xmax)?.0);

    // Substring traps specific to the new names: `cmin`/`cmax` sit inside no
    // other system column's name, but `xmax` is a suffix of nothing while
    // `max(...)` shares three letters with `cmax` — none of these is a match.
    for sql in [
        "SELECT max(id) FROM t",
        "SELECT min(id) FROM t",
        "SELECT xmin_backup FROM t",
        "SELECT t.ctid_hash FROM t",
    ] {
        for col in SysCol::ALL {
            let (bare, qualified) = demand_of_col(sql, col)?;
            assert!(
                !bare && qualified.is_empty(),
                "false positive for {} in `{sql}`",
                col.name(),
            );
        }
    }
    Ok(())
}

/// The slots are appended in [`SysCol::ALL`] order regardless of the order the
/// query names them in, because the executor fills them by walking that same
/// list. A demand that returned them in mention order would put a `Value::Xid`
/// in the `ctid` slot.
#[test]
fn demanded_columns_come_back_in_row_order() -> anyhow::Result<()> {
    let stmts = crabgresql_parser::parse("SELECT cmax, ctid, xmin, tableoid FROM t")
        .map_err(|error| anyhow!("invalid SQL test fixture: {error}"))?;
    let ast::Statement::Query(query) = &stmts[0] else {
        bail!("expected a query");
    };
    let ast::SetExpr::Select(select) = query.body.as_ref() else {
        bail!("expected a plain SELECT");
    };
    let demand = SystemDemand::of_select(select, &query.order_by);
    assert_eq!(
        demand.wants("t"),
        vec![SysCol::TableOid, SysCol::Ctid, SysCol::Xmin, SysCol::Cmax],
    );
    Ok(())
}
