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
//! It renders faithfully the constructs the regression suites exercise — a
//! `SELECT` list with aliases, `FROM`, `WHERE`, `GROUP BY`/`HAVING`,
//! `ORDER BY`, and the expression forms below — and falls back to the parser's
//! own `Display` for anything else it reaches. The fallback is syntactically
//! valid SQL but is *not* guaranteed to match PostgreSQL byte for byte, so
//! extend the match arms rather than relying on it.
//!
//! TODO: give the constructs outside that set their own match arms, so nothing
//! reaches the `Display` fallback and every definition is byte-identical to
//! `pg_get_viewdef`.
//!
//! TODO: render the parts that are dropped rather than fallen back on, since a
//! definition missing them no longer means what the view does: the `WITH`
//! list, `DISTINCT`, `LIMIT`/`OFFSET` and the `WINDOW` clause of the query,
//! and `DISTINCT`, `FILTER` and `OVER` on a function call.

use std::sync::Arc;

use crabgresql_parser::ast;
use crabgresql_storage_api::TypeCatalog;
use crabgresql_types::{FmtCtx, Numeric, PgType, Value, cast, interval, text, timestamptz};

use crate::expr::simple_body_select;

/// `pretty`-printed `pg_get_viewdef`. Returns `None` if `sql` does not re-parse
/// as a single `SELECT`, in which case the caller reports the view as
/// unavailable rather than emitting something misleading.
///
/// The odd-looking indentation is PostgreSQL's, not a typo: the body is indented
/// by one space, continuation lines of the select list by four, and each
/// subsequent clause keyword is right-aligned under `SELECT` — three spaces for
/// `FROM`, two for `ORDER BY`.
///
/// TODO: deparse set operations (`UNION`/`INTERSECT`/`EXCEPT`) and `VALUES`
/// bodies. They are the whole of the `None` case in practice: such a view can be
/// created and queried, but `pg_get_viewdef` raises `0A000` on it and
/// `pg_views.definition` reports NULL, so a dump cannot reproduce it. Pinned by
/// the `pg_views_undeparsable` smoke test — whose expected output is the only
/// one in that suite not taken from a live PostgreSQL, because PostgreSQL has no
/// such case to take it from.
pub fn view_definition(sql: &str, pretty: bool, columns: &[String]) -> Option<String> {
    let query = parse_query(sql)?;
    let ast::SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let cx = Cx {
        pretty,
        calls: None,
        zone: None,
        unqualify: None,
        domain_value: false,
        param_refs: false,
    };

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
        // and `HAVING` is a letter longer than the `WHERE`, `GROUP` and `ORDER`
        // that end in the same column.
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

/// See [`Cx::calls`].
type CallTypes<'a> = dyn Fn(&ast::Function) -> Option<Vec<PgType>> + 'a;

/// Rendering options that thread through the whole walk.
#[derive(Clone, Copy)]
struct Cx<'a> {
    /// `pg_get_viewdef`'s `pretty` flag, which is PG's `PRETTYFLAG_PAREN`: with
    /// it, an operator is parenthesised only where precedence actually requires
    /// it; without it, every operator node is wrapped.
    pretty: bool,
    /// Resolves a function call to the parameter types the binder chose for it,
    /// so a literal argument can carry the cast PostgreSQL's analysed tree would
    /// (`nextval('s'::regclass)`, not `nextval('s'::text)`).
    ///
    /// `None` reproduces the type-blind rendering: every bare string is `text`.
    /// A view takes that path, since deparsing one happens far from the binder;
    /// a column default takes the resolving path, because it is deparsed while
    /// the DDL that declared it is still being bound.
    calls: Option<&'a CallTypes<'a>>,
    /// The reading session's display zone, when there is one — see
    /// [`stored_expr`]. `None` leaves every constant as written.
    zone: Option<&'a FmtCtx>,
    /// The relation a CHECK constrains, when this walk is deparsing one.
    ///
    /// A CHECK binds in a scope holding exactly one relation, so `t.x` and `x`
    /// resolve to the same column and PostgreSQL prints the second — it stores
    /// bare `Var`s and has no qualifier left to print. Dropping ours matters for
    /// more than cosmetics: the stored text is re-bound against *inheritance
    /// children*, whose scope is qualified with the child's name, so a retained
    /// `parent.x` would fail there with 42P01.
    ///
    /// `None` everywhere else. A view's `t.c` really does select among several
    /// relations and must keep its qualifier.
    unqualify: Option<&'a str>,
    /// Whether this walk is deparsing a **domain** constraint, whose single
    /// operand is spelled `VALUE`.
    ///
    /// PostgreSQL renders that one in capitals — `CHECK ((VALUE > 0))` — and it
    /// is not an ordinary column reference at all but a placeholder for the
    /// value under test, so the usual lowercasing would be wrong twice over: it
    /// would not read back as PostgreSQL's text, and it would suggest a column
    /// that does not exist.
    domain_value: bool,
    /// Whether a bare identifier here names a **routine parameter** rather than
    /// a column — true for a SQL-standard body, whose scope holds no relation
    /// at all.
    ///
    /// Visible only under a subscript or a field access, where PostgreSQL
    /// parenthesises the container unless it is a plain column: a view prints
    /// `arr[1]`, a routine body `(a)[1]`. In PostgreSQL the two are different
    /// node types (a `Var` against a `Param`) and the rule reads off the node;
    /// crabgresql re-renders text, where both are identifiers, so which one is
    /// being deparsed has to be carried in.
    param_refs: bool,
}

/// Canonicalize an expression on its way *into* the catalog — a column default
/// or a CHECK constraint. Returns `None` if `sql` does not re-parse as one
/// expression, leaving the caller to fall back to the text as written.
///
/// `catalog` resolves the function calls inside, so a literal argument carries
/// the type its signature gives it.
///
/// The result is the *non-pretty* form, every operator node parenthesised —
/// `DEFAULT (1 + 2)` stays `(1 + 2)`, and `CHECK (x + y < 100)` stores
/// `((x + y) < 100)`. That is what `pg_get_expr` returns without its `pretty`
/// flag, and it is the form `information_schema.columns` echoes straight out of
/// the catalog; psql asks for the pretty one and [`stored_expr`] derives it by
/// re-parsing.
pub fn deparse_stored_expr(sql: &str, catalog: &Arc<dyn TypeCatalog>) -> Option<String> {
    deparse_into_catalog(sql, None, catalog, false)
}

/// [`deparse_stored_expr`] for a CHECK predicate, which additionally drops the
/// `relation.` qualifier from any column that carries one.
///
/// PostgreSQL stores bare `Var`s, so `CHECK (t.x > 0)` comes back as
/// `CHECK ((x > 0))`. Reproducing that is what lets the stored text be re-bound
/// against an inheritance child, whose scope answers to the child's name rather
/// than the parent's — see [`Cx::unqualify`].
pub fn deparse_check_expr(
    sql: &str,
    relation: &str,
    catalog: &Arc<dyn TypeCatalog>,
) -> Option<String> {
    deparse_into_catalog(sql, Some(relation), catalog, false)
}

/// [`deparse_check_expr`] for a **domain**'s predicate: there is no relation to
/// unqualify, and the operand is the `VALUE` placeholder.
pub fn deparse_domain_check_expr(sql: &str, catalog: &Arc<dyn TypeCatalog>) -> Option<String> {
    deparse_into_catalog(sql, None, catalog, true)
}

fn deparse_into_catalog(
    sql: &str,
    unqualify: Option<&str>,
    catalog: &Arc<dyn TypeCatalog>,
    domain_value: bool,
) -> Option<String> {
    let e = parse_expression(sql)?;
    let resolve = |f: &ast::Function| call_arg_types(f, catalog);
    let cx = Cx {
        pretty: false,
        calls: Some(&resolve),
        zone: None,
        unqualify,
        domain_value,
        param_refs: false,
    };
    Some(top_expr(&e, cx))
}

/// Render a stored `pg_node_tree` deparse for one reader — `pg_get_expr`'s side
/// of the split. `None` if `sql` is not a single expression (a partition bound,
/// say), leaving the caller to echo it as stored.
///
/// Two things about a default cannot be settled when it is written, because they
/// belong to whoever reads it:
///
/// * **Parenthesisation.** `pg_get_expr` wraps every operator node unless asked
///   to be `pretty`, so the *same* default is `(1 + 2)` to
///   `information_schema.columns` and `1 + 2` to psql's `\d`. Re-parsing is what
///   makes both available from one stored string.
/// * **The session zone.** A `timestamptz` constant renders in the reader's
///   zone, so one default reads `'2019-12-31 22:00:00+00'::timestamp with time
///   zone` under UTC and `'2020-01-01 07:00:00+09'::…` under `Asia/Tokyo`.
///   Verified against PostgreSQL 18.4, including that `timetz` does *not* move —
///   it carries its own offset — and neither does a zone-less `timestamp`.
///
/// This is idempotent on what the DDL path stores, because every literal there
/// already carries its cast: a re-render re-reads `'x'::text` as a cast node
/// rather than as the bare string it would have to guess a type for.
pub fn stored_expr(sql: &str, pretty: bool, fmt: &FmtCtx) -> Option<String> {
    stored_expr_of(sql, pretty, fmt, false)
}

/// [`stored_expr`] with the choice of whose predicate this is: a domain's, whose
/// `VALUE` placeholder renders in capitals, or a relation's.
pub fn stored_expr_of(sql: &str, pretty: bool, fmt: &FmtCtx, domain_value: bool) -> Option<String> {
    read_expr(sql, pretty, fmt, domain_value, false)
}

/// [`stored_expr`] for the `RETURN <expr>` form of a SQL-standard routine body.
///
/// The rendering is the non-pretty one a stored expression gets, with the one
/// difference [`Cx::param_refs`] describes: the identifiers are the routine's
/// parameters, so a subscript or a field access parenthesises its container.
pub fn sqlbody_expr(sql: &str, fmt: &FmtCtx) -> Option<String> {
    read_expr(sql, false, fmt, false, true)
}

fn read_expr(
    sql: &str,
    pretty: bool,
    fmt: &FmtCtx,
    domain_value: bool,
    param_refs: bool,
) -> Option<String> {
    let e = parse_expression(sql)?;
    let cx = Cx {
        pretty,
        calls: None,
        zone: Some(fmt),
        unqualify: None,
        domain_value,
        param_refs,
    };
    Some(top_expr(&e, cx))
}

/// A `'…'::timestamp with time zone` constant, re-rendered in this session's
/// zone. `None` for every other cast, which reads the same for everyone.
fn zoned_constant(inner: &ast::Expr, data_type: &ast::DataType, cx: Cx) -> Option<String> {
    let fmt = cx.zone?;
    if PgType::from_name(&type_name(data_type))? != PgType::TimestampTz {
        return None;
    }
    let ast::Expr::Value(v) = inner else {
        return None;
    };
    let ast::Value::SingleQuotedString(text) = &v.value else {
        return None;
    };
    let micros = timestamptz::parse(text, fmt).ok()?;
    Some(format!(
        "'{}'::timestamp with time zone",
        timestamptz::format(micros, &fmt.zone)
    ))
}

/// A `'…'::bytea` constant, re-rendered under this session's `bytea_output`.
/// `None` for every other cast.
///
/// The stored spelling is whichever the DDL session produced — `deparse_const`
/// writes hex, but a value that reached the catalog through a session-rendered
/// path can be escape — so the *input* side deliberately accepts both, which
/// `byteain` already does. That is also why this cannot be a string rewrite:
/// the two spellings have to go through the bytes to meet.
///
/// Verified against PostgreSQL 18.4, including the quoting: a `0x27` byte prints
/// verbatim in escape form and is then doubled inside the SQL literal, so
/// `'\x27615c'::bytea` re-renders as `'''a\\'::bytea`.
///
/// **Reaches column defaults, not CHECK predicates.** A CHECK is deparsed into
/// the catalog by [`deparse_check_expr`], which labels a bare literal `::text`
/// instead of resolving its type from the comparison — so `CHECK (b <>
/// '\x0061')` is stored as `'\x0061'::text` and arrives here typed `text`. That
/// mislabelling is type-general (a `date` constant in a CHECK prints `::text`
/// too, where PG prints `::date`) and predates this function.
///
/// TODO: resolve operand types during the deparse walk so a literal in a CHECK
/// carries the type its comparison gives it (`::bytea`, `::date`) instead of
/// `::text`; widening the match here would not reach it.
fn bytea_constant(inner: &ast::Expr, data_type: &ast::DataType, cx: Cx) -> Option<String> {
    let fmt = cx.zone?;
    if PgType::from_name(&type_name(data_type))? != PgType::Bytea {
        return None;
    }
    let ast::Expr::Value(v) = inner else {
        return None;
    };
    let ast::Value::SingleQuotedString(text) = &v.value else {
        return None;
    };
    let bytes = cast::byteain(text).ok()?;
    let rendered = Value::Bytea(bytes).encode_text_with(fmt)?;
    Some(format!(
        "{}::{}",
        value_body(&ast::Value::SingleQuotedString(rendered)),
        type_name(data_type)
    ))
}

/// The parameter types the binder resolves a call's arguments to, or `None` when
/// the call does not bind at all (a `CREATE FUNCTION` routine, say, which is not
/// in the built-in table). A default cannot reference columns, so binding it in
/// an empty scope is enough.
fn call_arg_types(f: &ast::Function, catalog: &Arc<dyn TypeCatalog>) -> Option<Vec<PgType>> {
    let params = crate::param_ctx_none();
    let scope = crate::Scope::empty(catalog, &params);
    let bound = crate::bind_expr(&ast::Expr::Function(f.clone()), &scope).ok()?;
    match bound {
        crate::Binding::Typed(crate::BoundExpr::FuncCall { args, .. }) => {
            Some(args.iter().map(|a| a.ty()).collect())
        }
        // `COALESCE`/`GREATEST`/`LEAST` coerce every argument to the one type they
        // resolved, so that is the label each literal argument carries: PG prints
        // `COALESCE(NULL::text, 'z'::text)`.
        crate::Binding::Typed(
            crate::BoundExpr::Coalesce { args, ty } | crate::BoundExpr::MinMax { args, ty, .. },
        ) => Some(vec![ty; args.len()]),
        // `NULLIF` binds to the `CASE` it is shorthand for, and both its
        // arguments were coerced to that expression's type — the one the `=`
        // operator settled on. PG prints `NULLIF('a'::text, 'b'::text)`.
        crate::Binding::Typed(crate::BoundExpr::Case { ty, .. }) if is_named(f, "nullif") => {
            Some(vec![ty; 2])
        }
        _ => None,
    }
}

pub fn parse_expression(sql: &str) -> Option<ast::Expr> {
    let query = parse_query(&format!("SELECT {sql}"))?;
    let ast::SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    match select.projection.as_slice() {
        [ast::SelectItem::UnnamedExpr(e)] => Some(e.clone()),
        _ => None,
    }
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
    if is_bare_column(e) {
        return None;
    }
    Some(figure_colname(e).unwrap_or_else(|| "?column?".to_string()))
}

/// Whether `e` selects a column under its own name, which is the one shape
/// `pg_get_viewdef` prints without an `AS`.
fn is_bare_column(e: &ast::Expr) -> bool {
    match e {
        ast::Expr::Identifier(_) | ast::Expr::CompoundIdentifier(_) => true,
        ast::Expr::Nested(inner) => is_bare_column(inner),
        _ => false,
    }
}

/// The name an unaliased expression names itself — PostgreSQL's
/// `FigureColname`. `None` is its `?column?` case: the expression suggests no
/// name at all, which the two readers spell differently (a view writes
/// `AS "?column?"`, a SQL body writes no alias).
fn figure_colname(e: &ast::Expr) -> Option<String> {
    colname_of(e).0
}

/// A name a node owns outright, as against one it merely offers. PostgreSQL's
/// `FigureColname` reports both, and the difference decides who wins when
/// names nest: a cast keeps its operand's name only if that name is `Owned`.
///
/// So `a::text` is `a` and `upper(a)::text` is `upper`, but `(a + 1)::text` is
/// `text` — and `1::int::text` is `text` rather than `int`, which is the case
/// a plain "recurse, else use the type" rule gets wrong. Verified on 18.4.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NameStrength {
    /// No name at all: the `?column?` case.
    None,
    /// A name an enclosing cast may override — a cast's own target type, or
    /// the `case` a `CASE` falls back on.
    Offered,
    /// A name taken from the thing itself: a column, a field, a function.
    Owned,
}

fn colname_of(e: &ast::Expr) -> (Option<String>, NameStrength) {
    let owned = |name: Option<String>| match name {
        Some(name) => (Some(name), NameStrength::Owned),
        None => (None, NameStrength::None),
    };
    match e {
        ast::Expr::Identifier(i) => owned(Some(name_of(i))),
        ast::Expr::CompoundIdentifier(parts) => owned(parts.last().map(name_of)),
        ast::Expr::Function(f) => owned(
            f.name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .map(|i| i.value.to_ascii_lowercase()),
        ),
        ast::Expr::Cast {
            expr: inner,
            data_type,
            ..
        } => match colname_of(inner) {
            (name, NameStrength::Owned) => (name, NameStrength::Owned),
            _ => (Some(type_name(data_type)), NameStrength::Offered),
        },
        ast::Expr::Case { else_result, .. } => match else_result.as_deref().map(colname_of) {
            Some((name, NameStrength::Owned)) => (name, NameStrength::Owned),
            _ => (Some("case".to_string()), NameStrength::Offered),
        },
        // A constructor's name survives a cast: `ARRAY[1, 2]::int[]` is `array`.
        ast::Expr::Array(_) => owned(Some("array".to_string())),
        ast::Expr::Tuple(_) => owned(Some("row".to_string())),
        ast::Expr::Nested(inner) => colname_of(inner),
        // A field access names itself after the field (`(p).x` is `x`); a
        // subscript keeps the name of what it indexes (`a[1]` is `a`), which is
        // why the chain is walked from the end rather than the root.
        ast::Expr::CompoundFieldAccess { root, access_chain } => {
            match access_chain.iter().rev().find_map(|a| match a {
                ast::AccessExpr::Dot(e) => Some(colname_of(e)),
                ast::AccessExpr::Subscript(_) => None,
            }) {
                Some(named) => named,
                None => colname_of(root),
            }
        }
        _ => (None, NameStrength::None),
    }
}

/// An identifier's column name: case-folded unless it was quoted, which is what
/// the parser did to it on the way in and therefore what PostgreSQL's stored
/// `resname` holds.
fn name_of(i: &ast::Ident) -> String {
    match i.quote_style {
        Some(_) => i.value.clone(),
        None => i.value.to_ascii_lowercase(),
    }
}

/// `pg_get_function_sqlbody`'s side of the select-list rendering: the single
/// statement of a `BEGIN ATOMIC` body, in the same non-pretty shape
/// [`view_definition`] uses, but under the *opposite* alias rule.
///
/// PostgreSQL prints `AS <resname>` on every target that names itself —
/// including a bare column, which a view leaves alone (`SELECT a AS a`) — and
/// no alias at all where a view would write `AS "?column?"` (`SELECT (a + b)`).
///
/// `None` where [`simple_body_select`] refuses the shape, which is the same
/// check `CREATE FUNCTION` validates against: a clause this renderer does not
/// know cannot reach it, and the caller echoes the text as stored rather than
/// print a body that means something else.
///
/// **Where a `CASE` target diverges from PostgreSQL:** PG breaks one across
/// lines and materialises the `ELSE NULL::<type>` its analyser inserted, both
/// of which need the typed tree this walk does not have. The alias (`AS "case"`)
/// does match, and [`view_definition`] has the same gap.
pub fn sqlbody_statement(sql: &str, fmt: &FmtCtx) -> Option<String> {
    let query = parse_query(sql)?;
    let select = simple_body_select(&query).ok()?;
    let cx = Cx {
        pretty: false,
        calls: None,
        zone: Some(fmt),
        unqualify: None,
        domain_value: false,
        // Unconditional because `simple_body_select` has already refused a
        // `FROM`: with no relation in scope, every identifier is a parameter.
        param_refs: true,
    };
    let projection: Vec<String> = select
        .projection
        .iter()
        .map(|item| match item {
            ast::SelectItem::UnnamedExpr(e) => match figure_colname(e) {
                Some(name) => Some(format!("{} AS {}", top_expr(e, cx), quote_name(&name))),
                None => Some(top_expr(e, cx)),
            },
            ast::SelectItem::ExprWithAlias { expr: e, alias } => {
                Some(format!("{} AS {}", top_expr(e, cx), ident(alias)))
            }
            // A `*` has no column list to expand against here, unlike a view's.
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some(format!("SELECT {}", projection.join(", ")))
}

fn from_item(from: &ast::TableWithJoins, _cx: Cx) -> String {
    if from.joins.is_empty() {
        table_factor(&from.relation)
    } else {
        // TODO: render a join in PG's parenthesised, indented shape; it keeps
        // the parser's rendering instead. Note it is emitted *verbatim* —
        // folding the case of the whole clause would rewrite quoted identifiers
        // and the contents of string literals inside the `ON`, turning a merely
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
        ast::Expr::Identifier(id) => match cx.domain_value && is_value_placeholder(id) {
            true => "VALUE".to_string(),
            false => ident(id),
        },
        // `relation.column` inside a CHECK loses its qualifier: see
        // [`Cx::unqualify`]. Guarded on the qualifier actually naming this
        // relation — a mismatched one is not ours to rewrite, and binding has
        // already rejected it with 42P01 by the time we deparse.
        ast::Expr::CompoundIdentifier(parts) => match (cx.unqualify, parts.as_slice()) {
            (Some(relation), [qualifier, column])
                if crate::expr::normalize_ident(qualifier) == relation =>
            {
                ident(column)
            }
            _ => parts.iter().map(ident).collect::<Vec<_>>().join("."),
        },
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
        // `a[1]`, `a[1:2]`, `(a).x` and any chain of the two. PostgreSQL
        // parenthesises the container of the *first* access unless that
        // container is a plain column and the access is a subscript — hence
        // `arr[1]` in a view against `(a)[1]` in a routine body, and `(p).x`
        // for a field access even off a column. Later links in the chain never
        // add parentheses: `(a)[1][2]`, `(p).x[1]`.
        ast::Expr::CompoundFieldAccess { root, access_chain } => {
            let field_first = matches!(access_chain.first(), Some(ast::AccessExpr::Dot(_)));
            let column_root = matches!(
                root.as_ref(),
                ast::Expr::Identifier(_) | ast::Expr::CompoundIdentifier(_)
            );
            let root = match cx.param_refs || field_first || !column_root {
                true => format!("({})", expr(root, cx, 0)),
                false => expr(root, cx, u8::MAX),
            };
            let chain: String = access_chain.iter().map(|a| access(a, cx)).collect();
            format!("{root}{chain}")
        }
        ast::Expr::Cast {
            expr: inner,
            data_type,
            ..
        } => zoned_constant(inner, data_type, cx)
            .or_else(|| bytea_constant(inner, data_type, cx))
            .unwrap_or_else(|| {
                // A constant carries exactly one type label. Rendering the operand
                // through `value` would add the `text` it assumes for a bare string
                // and produce `'x'::text::text`, which also makes re-rendering an
                // already-deparsed expression non-idempotent.
                //
                // Right for the literal already *of* the labelled type, which is
                // what PostgreSQL has left by deparse time — `'x'::text`. A
                // literal of another type is a coercion node there and prints
                // wrapped (`1::text` is `(1)::text`), which needs the operand
                // typed to tell apart and so is not reproduced here.
                match literal_of(inner) {
                    Some(v) => format!("{}::{}", value_body(v), type_name(data_type)),
                    // Everything else is a cast *node*, and PostgreSQL wraps
                    // its operand whenever it is not pretty-printing:
                    // `(a)::text`, `(now())::date`, and `((a + 1))::text`
                    // doubled up because an operator node already wraps itself
                    // there. Asking to be pretty drops both layers, which is
                    // what psql's `\d` shows.
                    None => {
                        let operand = expr(inner, cx, u8::MAX);
                        let ty = type_name(data_type);
                        match cx.pretty {
                            true => format!("{operand}::{ty}"),
                            false => format!("({operand})::{ty}"),
                        }
                    }
                }
            }),
        // A bare string literal in an analysed tree is never untyped: it carries
        // the cast the analyser resolved it to. `current_setting('TimeZone')`
        // deparses as `current_setting('TimeZone'::text)`.
        ast::Expr::Value(v) => value(&v.value),
        ast::Expr::Interval(iv) => interval_literal(iv),
        ast::Expr::Function(f) => function(f, cx),
        // Each branch is deparsed rather than echoed, so the operators inside
        // one follow the same wrapping rule as everywhere else:
        // `CASE WHEN (a > 0) THEN a ELSE (- a) END`. The `CASE` itself is never
        // parenthesised — it is atomic to its parent.
        ast::Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            let mut out = String::from("CASE");
            if let Some(operand) = operand {
                out.push_str(&format!(" {}", expr(operand, cx, 0)));
            }
            for when in conditions {
                out.push_str(&format!(
                    " WHEN {} THEN {}",
                    expr(&when.condition, cx, 0),
                    expr(&when.result, cx, 0)
                ));
            }
            if let Some(else_result) = else_result {
                out.push_str(&format!(" ELSE {}", expr(else_result, cx, 0)));
            }
            out.push_str(" END");
            out
        }
        // Walked rather than echoed so the rules that apply to an element apply
        // inside a constructor too — `ARRAY[(a)[1], 2]` in a routine body.
        ast::Expr::Array(arr) => {
            let elems: Vec<String> = arr.elem.iter().map(|e| top_expr(e, cx)).collect();
            let keyword = match arr.named {
                true => "ARRAY",
                false => "",
            };
            format!("{keyword}[{}]", elems.join(", "))
        }
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

/// One link of a subscript/field chain. The index is an expression like any
/// other and is deparsed as one — `arr[(i + 1)]`, not the `arr[i + 1]` echoing
/// the parser's own `Display` would give, which would also miss a qualifier
/// dropped by [`Cx::unqualify`] and a `timestamptz` constant's session zone.
///
/// A field name is not a column reference, so it is spelled as an identifier
/// rather than walked: `VALUE` and the CHECK qualifier have no business there.
fn access(a: &ast::AccessExpr, cx: Cx) -> String {
    match a {
        ast::AccessExpr::Dot(ast::Expr::Identifier(field)) => format!(".{}", ident(field)),
        ast::AccessExpr::Dot(other) => format!(".{other}"),
        ast::AccessExpr::Subscript(ast::Subscript::Index { index }) => {
            format!("[{}]", top_expr(index, cx))
        }
        ast::AccessExpr::Subscript(ast::Subscript::Slice {
            lower_bound,
            upper_bound,
            stride,
        }) => {
            let bound =
                |e: &Option<ast::Expr>| e.as_ref().map(|e| top_expr(e, cx)).unwrap_or_default();
            let mut out = format!("[{}:{}", bound(lower_bound), bound(upper_bound));
            // PostgreSQL has no stride, so nothing it prints reaches here; the
            // arm exists because another dialect's text can still parse into one.
            if let Some(stride) = stride {
                out.push_str(&format!(":{}", top_expr(stride, cx)));
            }
            out.push(']');
            out
        }
    }
}

fn function(f: &ast::Function, cx: Cx) -> String {
    // The `CURRENT_TIMESTAMP` family reaches the parser as a keyword, and PG
    // prints it back as one: upper case, and unparenthesized unless it carries
    // a precision. The generic path below would render `CURRENT_DATE` as
    // `current_date()`, which is neither.
    if let Some(rendered) = keyword_datetime(f) {
        return rendered;
    }
    // An argument's type comes from the signature the binder resolved, not from
    // the literal's own syntax — that is the whole difference between
    // `nextval('s'::regclass)` and `nextval('s'::text)`.
    let arg_types = cx.calls.and_then(|resolve| resolve(f));
    let arg_type = |i: usize| arg_types.as_ref().and_then(|types| types.get(i).copied());
    // `pg_typeof` coerces nothing: its argument keeps the type it was written
    // with, and that type *is* the result. So a literal argument takes no cast —
    // labelling a bare one `text` would change what the re-parsed expression
    // reports, and `unknown` has no spelling to label it with. PG prints
    // `pg_typeof('abc')`. Keyed on the name because this has to hold on the
    // type-blind path too (`pg_get_expr`, `pg_get_viewdef`), which has no binder.
    let bare_args = is_named(f, "pg_typeof");
    let args = match &f.args {
        ast::FunctionArguments::List(list) => list
            .args
            .iter()
            .enumerate()
            .map(|(i, a): (usize, &ast::FunctionArg)| {
                // `VARIADIC` is part of the call PG prints back, and the
                // argument under it deparses like any other.
                let (variadic, arg) = match a {
                    ast::FunctionArg::Unnamed(arg) => ("", arg),
                    ast::FunctionArg::Variadic(arg) => ("VARIADIC ", arg),
                    other => return other.to_string(),
                };
                let ast::FunctionArgExpr::Expr(e) = arg else {
                    return a.to_string();
                };
                let body = match (arg_type(i), literal_of(e)) {
                    (_, Some(v)) if bare_args => value_body(v),
                    (Some(ty), Some(v)) => typed_value(v, ty),
                    _ => top_expr(e, cx),
                };
                format!("{variadic}{body}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        ast::FunctionArguments::None => String::new(),
        other => return format!("{}{other}", object_name(&f.name)),
    };
    format!("{}({args})", call_name(f))
}

/// How PG spells a call back. `COALESCE`, `NULLIF`, `GREATEST` and `LEAST` are
/// grammar constructs, not functions, and PG prints them in upper case
/// (`COALESCE(1, 2)`) — everything else is a real function name and keeps its own
/// spelling. Keyed on the name so this holds on the type-blind `pg_get_expr` path
/// too.
fn call_name(f: &ast::Function) -> String {
    if ["coalesce", "nullif", "greatest", "least"]
        .iter()
        .any(|name| is_named(f, name))
        && let Some(ident) = f.name.0.last().and_then(|p| p.as_ident())
    {
        return ident.value.to_ascii_uppercase();
    }
    object_name(&f.name)
}

/// `CURRENT_DATE`, `CURRENT_TIME[(p)]`, `CURRENT_TIMESTAMP[(p)]`,
/// `LOCALTIME[(p)]`, `LOCALTIMESTAMP[(p)]` — how PostgreSQL prints them back.
/// `None` for any other call, including `now()` and `clock_timestamp()`, which
/// are real functions and print lower case with their parentheses.
fn keyword_datetime(f: &ast::Function) -> Option<String> {
    let name = f.name.0.last()?.as_ident()?.value.to_ascii_lowercase();
    if !matches!(
        name.as_str(),
        "current_date" | "current_time" | "current_timestamp" | "localtime" | "localtimestamp"
    ) {
        return None;
    }
    let upper = name.to_ascii_uppercase();
    match &f.args {
        ast::FunctionArguments::None => Some(upper),
        ast::FunctionArguments::List(list) => match list.args.as_slice() {
            [ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(p))] => {
                Some(format!("{upper}({p})"))
            }
            // The grammar admits nothing else, so this cannot be reached from
            // SQL; fall back to the generic rendering rather than assert.
            _ => None,
        },
        _ => None,
    }
}

/// The literal an expression is, if it is one — the only place a resolved type
/// changes the rendering, since every other node carries its own syntax.
fn literal_of(e: &ast::Expr) -> Option<&ast::Value> {
    match e {
        ast::Expr::Value(v) => Some(&v.value),
        _ => None,
    }
}

/// A literal rendered at the type its context gives it, rather than at the
/// `text` a bare string defaults to.
///
/// A bare `NULL` takes a label for the same reason a string does: it carries no type
/// of its own, so PostgreSQL writes the one the analyser gave it —
/// `COALESCE(NULL::integer, 5)`, `upper(NULL::text)`. Numbers and booleans spell
/// their own type and are left alone.
fn typed_value(v: &ast::Value, ty: PgType) -> String {
    match v {
        ast::Value::SingleQuotedString(_) | ast::Value::Null => {
            let label = ty
                .format_type(Some(-1))
                .unwrap_or_else(|| ty.name().to_string());
            format!("{}::{label}", value_body(v))
        }
        other => other.to_string(),
    }
}

/// A literal with no type label — the part every rendering of it shares.
fn value_body(v: &ast::Value) -> String {
    match v {
        ast::Value::SingleQuotedString(s) => format!("'{}'", s.replace('\'', "''")),
        other => other.to_string(),
    }
}

/// A literal, with the cast PostgreSQL's analyser attached to it. A bare string
/// is `text`; numbers and booleans need no cast.
fn value(v: &ast::Value) -> String {
    match v {
        ast::Value::SingleQuotedString(_) => format!("{}::text", value_body(v)),
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
    // TODO: re-render a qualified form (`INTERVAL '1' DAY`) as an interval
    // constant; its field information has no rendering here, so fall back to
    // the parser's shape rather than lose it.
    if iv.leading_field.is_some() || iv.last_field.is_some() {
        return ast::Expr::Interval(iv.clone()).to_string();
    }
    match interval::parse(&literal) {
        Ok(parsed) => format!("'{}'::interval", interval::format_verbose(parsed)),
        Err(_) => format!("'{literal}'::interval"),
    }
}

fn type_name(ty: &ast::DataType) -> String {
    // A cast label is spelled the way `format_type` spells it, modifier and all:
    // that is what makes `bpchar` print as `bpchar` rather than as the
    // `character` its error messages use, and `bit` come out quoted. Fall back to
    // the parser's rendering for a type we do not model (a `CREATE TYPE` name).
    let Some(t) = PgType::from_name(&ty.to_string().to_ascii_lowercase()) else {
        return ty.to_string().to_ascii_lowercase();
    };
    let typmod = match t {
        PgType::Numeric => crate::checked_numeric_typmod(ty)
            .ok()
            .flatten()
            .map(|(p, s)| Numeric::pack_typmod(p, s) + VARHDRSZ),
        PgType::Varchar | PgType::Bpchar => crate::checked_length_typmod(ty)
            .ok()
            .flatten()
            .map(|n| n + VARHDRSZ),
        PgType::Time | PgType::TimeTz | PgType::Timestamp | PgType::TimestampTz => {
            crate::datetime_precision(ty)
        }
        PgType::Interval => crate::interval_typmod(ty),
        _ => crate::checked_length_typmod(ty).ok().flatten(),
    };
    t.format_type(Some(typmod.unwrap_or(-1)))
        .unwrap_or_else(|| t.name().to_string())
}

/// The varlena header the character and numeric modifiers reserve, mirroring the
/// catalog's `atttypmod_of` encoding.
const VARHDRSZ: i32 = 4;

/// Whether a call names `want`, ignoring any `pg_catalog.` qualifier — the
/// deparser's own check, so it works without a binder.
fn is_named(f: &ast::Function, want: &str) -> bool {
    f.name
        .0
        .last()
        .and_then(|p| p.as_ident())
        .is_some_and(|id| id.quote_style.is_none() && id.value.eq_ignore_ascii_case(want))
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
/// that no longer parses: a shape outside `[a-z_][a-z0-9_]*`, and a name
/// spelled like a keyword — `SELECT 1 AS "select"` must not come back as
/// `SELECT 1 AS select`. [`text::quote_ident`] decides both, being PG's
/// `quote_identifier`; an *unquoted* name is folded first, since that is the
/// spelling the catalog holds and the one that has to read back.
/// Whether an identifier is the unquoted `VALUE` of a domain constraint.
/// Quoted `"value"` is an ordinary (and, in a domain, unresolvable) name.
fn is_value_placeholder(id: &ast::Ident) -> bool {
    id.quote_style.is_none() && id.value.eq_ignore_ascii_case("value")
}

fn ident(id: &ast::Ident) -> String {
    match id.quote_style {
        None => quote_name(&id.value.to_ascii_lowercase()),
        Some(_) => quote_name(&id.value),
    }
}

/// Render a bare name (a synthesised alias or an expanded wildcard column) with
/// the quoting it needs to read back as itself.
fn quote_name(name: &str) -> String {
    text::quote_ident(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pg_get_function_sqlbody`'s target list, pinned against PostgreSQL 18.4:
    /// every self-naming target carries an explicit `AS` — a bare column
    /// included, which is where this parts company with `pg_get_viewdef` — and
    /// an expression that names nothing carries none. `coalesce` is quoted
    /// because it is a keyword PostgreSQL quotes as an identifier.
    #[test]
    fn renders_a_sql_body_statement() {
        let fmt = FmtCtx::utc_default();
        let body = |sql| sqlbody_statement(sql, &fmt).expect("rendered");
        assert_eq!(body("SELECT a"), "SELECT a AS a");
        assert_eq!(body("SELECT a + b"), "SELECT (a + b)");
        assert_eq!(body("SELECT 1"), "SELECT 1");
        assert_eq!(body("SELECT upper(a)"), "SELECT upper(a) AS upper");
        assert_eq!(
            body("SELECT coalesce(a, 1)"),
            "SELECT COALESCE(a, 1) AS \"coalesce\""
        );
        assert_eq!(body("SELECT a + b AS z"), "SELECT (a + b) AS z");
    }

    /// A body this build cannot execute is reported as unrenderable rather than
    /// half-printed: the caller echoes the stored text instead. The shape comes
    /// from [`simple_body_select`], so a clause that would be dropped silently
    /// — `LIMIT` here — declines along with the rest.
    #[test]
    fn declines_a_sql_body_it_cannot_render() {
        let fmt = FmtCtx::utc_default();
        assert_eq!(sqlbody_statement("SELECT a FROM t", &fmt), None);
        assert_eq!(sqlbody_statement("SELECT 1 UNION SELECT 2", &fmt), None);
        assert_eq!(sqlbody_statement("INSERT INTO t VALUES (1)", &fmt), None);
        assert_eq!(sqlbody_statement("SELECT 1 LIMIT 1", &fmt), None);
        assert_eq!(sqlbody_statement("SELECT DISTINCT 1", &fmt), None);
        assert_eq!(
            sqlbody_statement("WITH c AS (SELECT 1) SELECT 1", &fmt),
            None
        );
    }

    /// PostgreSQL's `FigureColname`, which decides the `AS` a target carries.
    /// The cases that need its *strength* rule are the nested ones: a cast
    /// keeps a name its operand owns and overrides one merely offered, so
    /// `a::text` is `a` while `1::int::text` is `text` rather than `int`.
    /// Every string pinned against 18.4.
    #[test]
    fn names_a_target_the_way_figure_colname_does() {
        let name = |sql: &str| {
            let e = parse_expression(sql).expect("parsed");
            figure_colname(&e)
        };
        assert_eq!(name("a::text").as_deref(), Some("a"));
        assert_eq!(name("upper(a)::text").as_deref(), Some("upper"));
        assert_eq!(name("(a + 1)::text").as_deref(), Some("text"));
        assert_eq!(name("1::int::text").as_deref(), Some("text"));
        assert_eq!(name("arr[i + 1]").as_deref(), Some("arr"));
        assert_eq!(name("(arr[1])::text").as_deref(), Some("arr"));
        assert_eq!(name("CASE WHEN a > 0 THEN a END").as_deref(), Some("case"));
        assert_eq!(
            name("CASE WHEN true THEN 1 ELSE a END").as_deref(),
            Some("a")
        );
        assert_eq!(
            name("(CASE WHEN a > 0 THEN a END)::text").as_deref(),
            Some("text")
        );
        assert_eq!(name("ARRAY[1, 2]").as_deref(), Some("array"));
        assert_eq!(name("ARRAY[1, 2]::int[]").as_deref(), Some("array"));
        assert_eq!(name("a + b"), None);
    }

    /// A cast node's operand is parenthesised when not pretty-printing, and an
    /// operator node inside one doubles up because it already wraps itself. A
    /// *literal* keeps its bare label — it is a constant carrying its own type,
    /// not a coercion. Pinned against 18.4's `pg_get_expr`.
    #[test]
    fn parenthesises_a_cast_operand() {
        let fmt = FmtCtx::utc_default();
        let plain = |sql: &str| stored_expr(sql, false, &fmt).expect("rendered");
        let pretty = |sql: &str| stored_expr(sql, true, &fmt).expect("rendered");
        assert_eq!(plain("a::text"), "(a)::text");
        assert_eq!(plain("(a + 1)::text"), "((a + 1))::text");
        assert_eq!(plain("upper(a::text)"), "upper((a)::text)");
        assert_eq!(plain("arr[1]::text"), "(arr[1])::text");
        assert_eq!(plain("'x'::text"), "'x'::text");
        assert_eq!(plain("NULL::text"), "NULL::text");
        assert_eq!(pretty("a::text"), "a::text");
        assert_eq!(pretty("(a + 1)::text"), "(a + 1)::text");
    }

    /// A subscript's index is an expression like any other, so it is deparsed
    /// rather than echoed: `arr[(i + 1)]`, as 18.4 prints it.
    #[test]
    fn deparses_a_subscript_index() {
        let fmt = FmtCtx::utc_default();
        let plain = |sql: &str| stored_expr(sql, false, &fmt).expect("rendered");
        assert_eq!(plain("arr[i + 1]"), "arr[(i + 1)]");
        assert_eq!(plain("arr[1]"), "arr[1]");
        assert_eq!(plain("ARRAY[arr[1], 2]"), "ARRAY[arr[1], 2]");
    }

    /// A `CASE` is deparsed branch by branch, so the operators inside one are
    /// wrapped like any other. Pinned against PostgreSQL 18.4's
    /// `pg_get_function_sqlbody`, which prints the whole construct on one line.
    #[test]
    fn deparses_a_case_expression() {
        let fmt = FmtCtx::utc_default();
        assert_eq!(
            stored_expr("CASE WHEN a > 0 THEN a ELSE -a END", false, &fmt).as_deref(),
            Some("CASE WHEN (a > 0) THEN a ELSE (- a) END")
        );
        assert_eq!(
            stored_expr("CASE a WHEN 1 THEN a + 1 ELSE a * 2 END", false, &fmt).as_deref(),
            Some("CASE a WHEN 1 THEN (a + 1) ELSE (a * 2) END")
        );
    }

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

    /// Quoting follows the keyword's *category*, not whether it was written
    /// quoted: `numeric` and `between` are keywords PG quotes even though they
    /// read fine as bare aliases, while `value` and `name` are unreserved and
    /// stay bare. Pinned against PostgreSQL 18.4's `pg_get_viewdef`.
    #[test]
    fn quotes_a_keyword_alias_that_was_written_bare() {
        let sql = "SELECT 1 AS numeric, 2 AS value, 3 AS between, 4 AS name FROM t";
        let want = " SELECT 1 AS \"numeric\",\n\
                    \x20   2 AS value,\n\
                    \x20   3 AS \"between\",\n\
                    \x20   4 AS name\n\
                    \x20  FROM t;";
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
