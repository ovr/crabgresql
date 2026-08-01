//! Re-rendering a stored view definition the way `pg_get_viewdef` does.
//!
//! Clean-room (see AGENTS.md): this reproduces the *observable* text PostgreSQL
//! returns — its indentation, its parenthesisation, its explicit casts — pinned
//! against a live server, implemented independently.
//!
//! # Why re-render at all
//!
//! A view stores the SQL its `CREATE VIEW` was written with, so handing that
//! text back would be easy. PostgreSQL does not: it deparses the *analysed*
//! tree, so what comes back is canonical rather than as-typed — identifiers
//! folded to lower case, every column given an explicit `AS`, literals carrying
//! the cast that fixes their type. Clients (`pg_dump`, `psql \d+`) depend on
//! that canonical shape, so we re-parse the stored text and render it the same
//! way.
//!
//! # Scope
//!
//! This is deliberately **partial**. It renders faithfully the constructs the
//! regression suites exercise — a `SELECT` list with aliases, `FROM`, `WHERE`,
//! `GROUP BY`/`HAVING`, `ORDER BY`, and the expression forms below — and falls
//! back to the parser's own `Display` for anything else. The fallback is
//! syntactically valid SQL but is *not* guaranteed to match PostgreSQL byte for
//! byte, so extend the match arms rather than relying on it.

use crabgresql_parser::{ast, keywords};
use crabgresql_types::{PgType, interval, text};

/// `pretty`-printed `pg_get_viewdef`. Returns `None` if `sql` does not re-parse
/// as a single `SELECT`, in which case the caller reports the view as
/// unavailable rather than emitting something misleading.
///
/// The odd-looking indentation is PostgreSQL's, not a typo: the body is indented
/// by one space, continuation lines of the select list by four, and each
/// subsequent clause keyword is right-aligned under `SELECT` — three spaces for
/// `FROM`, two for `ORDER BY`.
pub fn view_definition(sql: &str, pretty: bool, columns: &[String]) -> Option<String> {
    let query = parse_query(sql)?;
    let ast::SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let cx = Cx { pretty };

    let mut out = String::from(" SELECT ");
    let projection: Vec<String> = select
        .projection
        .iter()
        .flat_map(|item| select_item(item, cx, columns))
        .collect();
    out.push_str(&projection.join(",\n    "));

    if !select.from.is_empty() {
        let froms: Vec<String> = select.from.iter().map(|f| from_item(f, cx)).collect();
        out.push_str(&format!("\n   FROM {}", froms.join(",\n    ")));
    }
    if let Some(sel) = &select.selection {
        out.push_str(&format!("\n  WHERE {}", top_expr(sel, cx)));
    }
    if let ast::GroupByExpr::Expressions(exprs, _) = &select.group_by
        && !exprs.is_empty()
    {
        let keys: Vec<String> = exprs.iter().map(|e| top_expr(e, cx)).collect();
        out.push_str(&format!("\n  GROUP BY {}", keys.join(", ")));
    }
    if let Some(having) = &select.having {
        // One space, not two: PG right-aligns the clause keyword under `SELECT`,
        // and `HAVING` is a letter longer than `WHERE`/`GROUP BY`/`ORDER BY`.
        out.push_str(&format!("\n HAVING {}", top_expr(having, cx)));
    }
    if let Some(order_by) = &query.order_by
        && let ast::OrderByKind::Expressions(exprs) = &order_by.kind
    {
        let keys: Vec<String> = exprs.iter().map(|o| order_by_expr(o, cx)).collect();
        out.push_str(&format!("\n  ORDER BY {}", keys.join(", ")));
    }
    out.push(';');
    Some(out)
}

/// Rendering options that thread through the whole walk.
#[derive(Clone, Copy)]
struct Cx {
    /// `pg_get_viewdef`'s `pretty` flag, which is PG's `PRETTYFLAG_PAREN`: with
    /// it, an operator is parenthesised only where precedence actually requires
    /// it; without it, every operator node is wrapped.
    pretty: bool,
}

fn parse_query(sql: &str) -> Option<Box<ast::Query>> {
    let dialect = crabgresql_parser::dialect::PostgreSqlDialect {};
    let mut stmts = crabgresql_parser::parser::Parser::parse_sql(&dialect, sql).ok()?;
    if stmts.len() != 1 {
        return None;
    }
    match stmts.remove(0) {
        ast::Statement::Query(q) => Some(q),
        _ => None,
    }
}

/// One select-list entry, as zero or more rendered items (a wildcard expands to
/// several).
///
/// PostgreSQL writes an explicit `AS` on every item *except* a bare column
/// reference already called by its own name — `SELECT a` stays `a`, while
/// `upper('x')` becomes `upper('x'::text) AS upper`. An expression with no
/// natural name is `AS "?column?"`.
fn select_item(item: &ast::SelectItem, cx: Cx, columns: &[String]) -> Vec<String> {
    match item {
        ast::SelectItem::UnnamedExpr(e) => {
            let rendered = top_expr(e, cx);
            match implicit_name(e) {
                // A bare column keeps its own name, so PG omits the alias.
                None => vec![rendered],
                Some(name) => vec![format!("{rendered} AS {}", quote_name(&name))],
            }
        }
        ast::SelectItem::ExprWithAlias { expr: e, alias } => {
            vec![format!("{} AS {}", top_expr(e, cx), ident(alias))]
        }
        // A wildcard is expanded to the column list frozen at `CREATE VIEW`
        // time, as PG does — leaving the `*` in place would let a later
        // `ALTER TABLE ADD COLUMN` silently widen the replayed definition.
        ast::SelectItem::Wildcard(_) | ast::SelectItem::QualifiedWildcard(..) => {
            columns.iter().map(|c| quote_name(c)).collect()
        }
        other => vec![other.to_string()],
    }
}

/// The name PG would give an unaliased select item, or `None` when the
/// expression is a bare column reference (which needs no `AS` at all).
fn implicit_name(e: &ast::Expr) -> Option<String> {
    match e {
        ast::Expr::Identifier(_) | ast::Expr::CompoundIdentifier(_) => None,
        ast::Expr::Function(f) => Some(
            f.name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .map(|i| i.value.to_ascii_lowercase())
                .unwrap_or_else(|| "?column?".to_string()),
        ),
        ast::Expr::Cast { data_type, .. } => Some(type_name(data_type)),
        ast::Expr::Nested(inner) => implicit_name(inner),
        _ => Some("?column?".to_string()),
    }
}

fn from_item(from: &ast::TableWithJoins, _cx: Cx) -> String {
    if from.joins.is_empty() {
        table_factor(&from.relation)
    } else {
        // A join keeps the parser's rendering: PG's parenthesised, indented
        // shape is not reproduced here. Note it is emitted *verbatim* — folding
        // the case of the whole clause would rewrite quoted identifiers and the
        // contents of string literals inside the `ON`, turning a merely
        // non-canonical rendering into SQL that no longer means the same thing.
        from.to_string()
    }
}

fn table_factor(factor: &ast::TableFactor) -> String {
    match factor {
        ast::TableFactor::Table { name, alias, .. } => {
            let mut s = object_name(name);
            if let Some(a) = alias {
                s.push_str(&format!(" {}", ident(&a.name)));
            }
            s
        }
        other => other.to_string(),
    }
}

/// One `ORDER BY` key. PG omits everything that is already the default, so only
/// a non-default direction or null-ordering is written out — and the default
/// null-ordering depends on the direction: `NULLS LAST` for `ASC`, `NULLS FIRST`
/// for `DESC`. So `ORDER BY a DESC NULLS FIRST` deparses as plain
/// `ORDER BY a DESC`, while `ORDER BY a NULLS FIRST` keeps its clause.
fn order_by_expr(o: &ast::OrderByExpr, cx: Cx) -> String {
    let mut s = top_expr(&o.expr, cx);
    let descending = o.options.asc == Some(false);
    if descending {
        s.push_str(" DESC");
    }
    if let Some(nulls_first) = o.options.nulls_first
        && nulls_first != descending
    {
        s.push_str(if nulls_first {
            " NULLS FIRST"
        } else {
            " NULLS LAST"
        });
    }
    s
}

/// An expression in `ruleutils` shape.
///
/// Two rules drive most of the differences from the parser's own `Display`:
/// PostgreSQL parenthesises any operator-like construct that is not a plain
/// function call, and it gives every literal an explicit cast to the type the
/// analyser gave it.
/// An expression at a clause boundary, where nothing encloses it — so in pretty
/// mode even a top-level operator needs no parentheses.
fn top_expr(e: &ast::Expr, cx: Cx) -> String {
    expr(e, cx, 0)
}

/// Binding power, ordered as PG's grammar does. Only the relative order matters:
/// a child binding *less* tightly than its parent needs parentheses.
fn precedence(e: &ast::Expr) -> u8 {
    use ast::BinaryOperator as B;
    match e {
        ast::Expr::BinaryOp { op, .. } => match op {
            B::Or => 1,
            B::And => 2,
            B::Eq | B::NotEq | B::Lt | B::LtEq | B::Gt | B::GtEq => 4,
            B::Plus | B::Minus => 6,
            B::Multiply | B::Divide | B::Modulo => 7,
            _ => 5,
        },
        ast::Expr::UnaryOp { op, .. } => match op {
            ast::UnaryOperator::Not => 3,
            _ => 8,
        },
        ast::Expr::IsTrue(_)
        | ast::Expr::IsNotTrue(_)
        | ast::Expr::IsFalse(_)
        | ast::Expr::IsNotFalse(_)
        | ast::Expr::IsNull(_)
        | ast::Expr::IsNotNull(_) => 4,
        ast::Expr::AtTimeZone { .. } | ast::Expr::AtLocal { .. } => 9,
        // Anything atomic (literal, column, function call, parenthesised group)
        // never needs wrapping.
        _ => u8::MAX,
    }
}

/// An expression rendered in `ruleutils` shape.
///
/// `parent` is the binding power of the enclosing operator, so a child that
/// binds less tightly gets parentheses. In non-pretty mode PG wraps every
/// operator node regardless, which is what `cx.pretty == false` reproduces.
fn expr(e: &ast::Expr, cx: Cx, parent: u8) -> String {
    let prec = precedence(e);
    let body = match e {
        ast::Expr::Identifier(id) => ident(id),
        ast::Expr::CompoundIdentifier(parts) => {
            parts.iter().map(ident).collect::<Vec<_>>().join(".")
        }
        ast::Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => format!(
            "{} AT TIME ZONE {}",
            expr(timestamp, cx, prec),
            expr(time_zone, cx, prec)
        ),
        ast::Expr::AtLocal { timestamp } => format!("{} AT LOCAL", expr(timestamp, cx, prec)),
        ast::Expr::BinaryOp {
            left, op, right, ..
        } => format!(
            "{} {op} {}",
            expr(left, cx, prec),
            // The right operand of a left-associative operator needs a wrap at
            // equal precedence (`a - (b - c)`), so ask it for one more.
            expr(right, cx, prec + 1)
        ),
        // PG puts a space after a unary operator: `- a`, not `-a`.
        ast::Expr::UnaryOp { op, expr: inner } => format!("{op} {}", expr(inner, cx, prec)),
        // A redundant group in the source is dropped; precedence alone decides
        // where parentheses land in the output.
        ast::Expr::Nested(inner) => return expr(inner, cx, parent),
        ast::Expr::Cast {
            expr: inner,
            data_type,
            ..
        } => format!("{}::{}", expr(inner, cx, u8::MAX), type_name(data_type)),
        // A bare string literal in an analysed tree is never untyped: it carries
        // the cast the analyser resolved it to. `current_setting('TimeZone')`
        // deparses as `current_setting('TimeZone'::text)`.
        ast::Expr::Value(v) => value(&v.value),
        ast::Expr::Interval(iv) => interval_literal(iv),
        ast::Expr::Function(f) => function(f, cx),
        other => other.to_string(),
    };
    // `AT TIME ZONE` / `AT LOCAL` are the one construct PG parenthesises even
    // when pretty-printing, so they are wrapped unconditionally.
    let always = matches!(e, ast::Expr::AtTimeZone { .. } | ast::Expr::AtLocal { .. });
    let wrap = if cx.pretty {
        always || prec < parent
    } else {
        prec != u8::MAX
    };
    if wrap { format!("({body})") } else { body }
}

fn function(f: &ast::Function, cx: Cx) -> String {
    let args = match &f.args {
        ast::FunctionArguments::List(list) => list
            .args
            .iter()
            .map(|a: &ast::FunctionArg| match a {
                ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => top_expr(e, cx),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        ast::FunctionArguments::None => String::new(),
        other => return format!("{}{other}", object_name(&f.name)),
    };
    format!("{}({args})", object_name(&f.name))
}

/// A literal, with the cast PostgreSQL's analyser attached to it. A bare string
/// is `text`; numbers and booleans need no cast.
fn value(v: &ast::Value) -> String {
    match v {
        ast::Value::SingleQuotedString(s) => format!("'{}'::text", s.replace('\'', "''")),
        other => other.to_string(),
    }
}

/// An `INTERVAL 'x'` literal. PostgreSQL renders it as a *constant of interval
/// type*, not as the `INTERVAL` syntax — and in `postgres_verbose` style
/// regardless of the session's `IntervalStyle`, so a dumped definition reads the
/// same everywhere. Hence `INTERVAL '00:00'` comes back as `'@ 0'::interval`.
fn interval_literal(iv: &ast::Interval) -> String {
    let literal = match iv.value.as_ref() {
        ast::Expr::Value(v) => match &v.value {
            ast::Value::SingleQuotedString(s) => s.clone(),
            other => other.to_string(),
        },
        other => return format!("{other}"),
    };
    // A qualified form (`INTERVAL '1' DAY`) has field information we do not
    // re-render; fall back to the parser's shape rather than lose it.
    if iv.leading_field.is_some() || iv.last_field.is_some() {
        return ast::Expr::Interval(iv.clone()).to_string();
    }
    match interval::parse(&literal) {
        Ok(parsed) => format!("'{}'::interval", interval::format_verbose(parsed)),
        Err(_) => format!("'{literal}'::interval"),
    }
}

fn type_name(ty: &ast::DataType) -> String {
    // Prefer our own canonical spelling, which matches `pg_type.typname`
    // handling elsewhere; fall back to the parser's rendering for a type we do
    // not model (a `CREATE TYPE` name).
    match PgType::from_name(&ty.to_string().to_ascii_lowercase()) {
        Some(t) => t.name().to_string(),
        None => ty.to_string().to_ascii_lowercase(),
    }
}

fn object_name(name: &ast::ObjectName) -> String {
    name.0
        .iter()
        .map(|p: &ast::ObjectNamePart| match p.as_ident() {
            Some(id) => ident(id),
            None => p.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// An identifier, folded the way PostgreSQL folds it. An unquoted name is
/// already lower case in the catalog, so it prints bare; a quoted one keeps its
/// quotes whenever dropping them would change what it means.
///
/// "Would change what it means" covers two cases, and missing either emits SQL
/// that no longer parses: a shape outside `[a-z_][a-z0-9_]*`, which
/// [`text::quote_ident`] already decides, and a name spelled like a keyword —
/// `SELECT 1 AS "select"` must not come back as `SELECT 1 AS select`.
fn ident(id: &ast::Ident) -> String {
    if id.quote_style.is_none() {
        return id.value.to_ascii_lowercase();
    }
    if is_reserved(&id.value) {
        return format!("\"{}\"", id.value.replace('"', "\"\""));
    }
    text::quote_ident(&id.value)
}

/// Render a bare name (a synthesised alias or an expanded wildcard column) with
/// the quoting it needs to read back as itself.
fn quote_name(name: &str) -> String {
    if is_reserved(name) {
        return format!("\"{}\"", name.replace('"', "\"\""));
    }
    text::quote_ident(name)
}

/// Whether `name` is a keyword that must be quoted to be used as an identifier.
///
/// Not *every* keyword: PG quotes only those whose category is not
/// `UNRESERVED_KEYWORD`, so ordinary aliases like `type`, `value`, `name` and
/// `day` stay bare while `select` does not. The parser's two reserved lists are
/// the same distinction from the other side — the words it refuses to read as a
/// bare alias.
fn is_reserved(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let Some(kw) = keywords::ALL_KEYWORDS
        .binary_search(&upper.as_str())
        .ok()
        .map(|i| keywords::ALL_KEYWORDS_INDEX[i])
    else {
        return false;
    };
    keywords::RESERVED_FOR_COLUMN_ALIAS.contains(&kw)
        || keywords::RESERVED_FOR_TABLE_ALIAS.contains(&kw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned against PostgreSQL 18.4, with the interval constant adjusted to
    /// the `postgres_verbose` spelling the vendored regression files expect.
    #[test]
    fn deparses_the_timetz_view() {
        let sql = "SELECT f1 AS dat, \
                   timezone(f1) AS dat_func, \
                   f1 AT LOCAL AS dat_at_local, \
                   f1 AT TIME ZONE current_setting('TimeZone') AS dat_at_tz, \
                   f1 AT TIME ZONE INTERVAL '00:00' AS dat_at_int \
                   FROM TIMETZ_TBL ORDER BY f1";
        let want = " SELECT f1 AS dat,\n\
                    \x20   timezone(f1) AS dat_func,\n\
                    \x20   (f1 AT LOCAL) AS dat_at_local,\n\
                    \x20   (f1 AT TIME ZONE current_setting('TimeZone'::text)) AS dat_at_tz,\n\
                    \x20   (f1 AT TIME ZONE '@ 0'::interval) AS dat_at_int\n\
                    \x20  FROM timetz_tbl\n\
                    \x20 ORDER BY f1;";
        assert_eq!(view_definition(sql, true, &[]).as_deref(), Some(want));
    }

    /// Quoting and ORDER BY defaults, pinned against PostgreSQL 18.4. A quoted
    /// keyword must keep its quotes or the text stops parsing; a null-ordering
    /// that matches the direction's default is dropped, as `ASC` is.
    #[test]
    fn quotes_keywords_and_drops_default_orderings() {
        let sql = "SELECT 1 AS \"select\", 2 AS \"Mixed\", a AS plain \
                   FROM t ORDER BY a DESC NULLS FIRST";
        let want = " SELECT 1 AS \"select\",\n\
                    \x20   2 AS \"Mixed\",\n\
                    \x20   a AS plain\n\
                    \x20  FROM t\n\
                    \x20 ORDER BY a DESC;";
        assert_eq!(view_definition(sql, true, &[]).as_deref(), Some(want));
    }

    /// A non-default null-ordering is kept, unlike the default one above.
    #[test]
    fn keeps_non_default_null_ordering() {
        let out = view_definition("SELECT a FROM t ORDER BY a NULLS FIRST", true, &[])
            .unwrap_or_default();
        assert!(out.ends_with("ORDER BY a NULLS FIRST;"), "got {out}");
        let out = view_definition("SELECT a FROM t ORDER BY a DESC NULLS LAST", true, &[])
            .unwrap_or_default();
        assert!(out.ends_with("ORDER BY a DESC NULLS LAST;"), "got {out}");
    }

    /// Operator parenthesisation, alias synthesis, wildcard expansion, keyword
    /// quoting and the `HAVING` indent, all pinned against PostgreSQL 18.4.
    #[test]
    fn matches_pg_on_operators_aliases_and_wildcards() {
        // Pretty mode drops the parentheses that only restate precedence, and
        // keeps the ones that override it.
        let sql = "SELECT a+b AS s, (a>b) AS c, -a AS n, (a+b)*a AS x, a+b*a AS y, \
                   a AS type, upper('x') FROM t WHERE a>1 AND b<2";
        let want = " SELECT a + b AS s,\n\
                    \x20   a > b AS c,\n\
                    \x20   - a AS n,\n\
                    \x20   (a + b) * a AS x,\n\
                    \x20   a + b * a AS y,\n\
                    \x20   a AS type,\n\
                    \x20   upper('x'::text) AS upper\n\
                    \x20  FROM t\n\
                    \x20 WHERE a > 1 AND b < 2;";
        assert_eq!(view_definition(sql, true, &[]).as_deref(), Some(want));

        // Without `pretty`, PG wraps every operator node.
        let want_flat = " SELECT (a + b) AS s,\n\
                         \x20   (a > b) AS c,\n\
                         \x20   (- a) AS n,\n\
                         \x20   ((a + b) * a) AS x,\n\
                         \x20   (a + (b * a)) AS y,\n\
                         \x20   a AS type,\n\
                         \x20   upper('x'::text) AS upper\n\
                         \x20  FROM t\n\
                         \x20 WHERE ((a > 1) AND (b < 2));";
        assert_eq!(view_definition(sql, false, &[]).as_deref(), Some(want_flat));

        // `HAVING` is right-aligned under SELECT, so one space, not two.
        let having = view_definition("SELECT a FROM t GROUP BY a HAVING sum(b) > 2", true, &[])
            .unwrap_or_default();
        assert!(
            having.ends_with("\n  GROUP BY a\n HAVING sum(b) > 2;"),
            "got {having}"
        );

        // A wildcard expands to the columns frozen at CREATE VIEW time.
        let cols = ["a".to_string(), "b".to_string()];
        assert_eq!(
            view_definition("SELECT * FROM t", true, &cols).as_deref(),
            Some(" SELECT a,\n    b\n   FROM t;")
        );
    }

    /// A body this deparser cannot render reports `None` rather than guessing.
    /// The caller turns that into an error, never into the empty string, which
    /// is reserved for "this relation is not a view".
    #[test]
    fn unsupported_body_is_not_silently_empty() {
        assert_eq!(view_definition("SELECT 1 UNION SELECT 2", true, &[]), None);
        assert_eq!(view_definition("VALUES (1), (2)", true, &[]), None);
    }

    #[test]
    fn unparseable_sql_has_no_definition() {
        assert_eq!(view_definition("not a query at all", true, &[]), None);
    }
}
