//! `reg*` input and output: turning an object name into an OID and back.
//!
//! PostgreSQL keeps only the OID in a `reg*` value and resolves the name in the
//! type's output function. `Value::encode_text` here is pure, so resolution
//! happens when the value is *built* — in this module — and the rendered name
//! travels with it (see [`crabgresql_types::Reg`]).
//!
//! Every rendering below was probed against PostgreSQL 18.4.

use crabgresql_pg_wire::sqlstate;
use crabgresql_types::{PgType, Reg, RegKind, text::quote_ident};

use crate::{CatalogOps, ExecError};

/// Build the `reg*` value an OID denotes, resolving its name against the
/// catalog. An OID that names nothing is not an error: PG renders `0` as `-` and
/// any other unresolvable OID as its bare digits.
pub fn from_oid(kind: RegKind, oid: u32, ops: &dyn CatalogOps) -> Reg {
    match render(kind, oid, ops) {
        Some(name) => Reg { kind, oid, name },
        None => Reg::unresolved(kind, oid),
    }
}

/// The name an OID renders as, or `None` if it resolves to nothing.
fn render(kind: RegKind, oid: u32, ops: &dyn CatalogOps) -> Option<String> {
    if oid == 0 {
        return None;
    }
    match kind {
        // A function prints under its bare name. PG schema-qualifies one that
        // an unqualified name would not reach; built-ins live in `pg_catalog`
        // and every `CREATE FUNCTION` routine lands in `public`.
        // TODO: schema-qualify a regproc name the session's search path does
        // not reach.
        RegKind::Proc => ops.proc_name(oid),
        // An operator prints bare only when its bare name would read *back* as
        // this same operator — the round trip `regoperin` would make. `=` names
        // some ninety operators, so most of them print schema-qualified even
        // though `pg_catalog` is always on the search path.
        RegKind::Oper => {
            let (namespace, name) = ops.oper_name(oid)?;
            Some(match ops.oper_oids(None, &name).as_slice() {
                [only] if *only == oid => name,
                // An operator name is punctuation, never an identifier, so only
                // the schema is quoted: `pg_catalog.+`.
                _ => format!("{}.{}", quote_ident(&namespace), name),
            })
        }
        // A relation is printed bare when an unqualified name reaches it, and
        // schema-qualified when it does not — the same reachability rule
        // `pg_table_is_visible` answers, so the two can never disagree.
        RegKind::Class => {
            let (namespace, name) = ops.rel_name(oid)?;
            Some(match ops.table_is_visible(oid) {
                Some(true) => quote_ident(&name),
                _ => format!("{}.{}", quote_ident(&namespace), quote_ident(&name)),
            })
        }
        // `regtype` prints a built-in under its SQL spelling, not its catalog
        // one: 23 is `integer`, 1005 is `smallint[]`, 1043 is
        // `character varying`. That is exactly `PgType::name`.
        RegKind::Type => match PgType::from_oid(oid) {
            Some(ty) => Some(ty.name().to_string()),
            // A pseudo-type has a catalog row but no `PgType`, so it names itself
            // from the shared table — `pg_typeof` reports `unknown` for an untyped
            // literal, and `record`/`void`/`anyelement` turn up in introspection.
            // Ahead of the user-type lookup: these OIDs are all below the
            // user-OID floor, so the order is belt-and-braces.
            None => crabgresql_types::pseudo_type_name(oid)
                .map(str::to_string)
                .or_else(|| ops.user_type_name(oid).map(|(_, name)| quote_ident(&name))),
        },
        RegKind::Namespace => ops.namespace_name(oid).map(|n| quote_ident(&n)),
    }
}

/// Resolve a `reg*` input string to a value, as PG's `regclassin` and friends
/// do. All digits is an OID written directly; anything else is an object name,
/// optionally schema-qualified and optionally quoted.
pub fn from_text(kind: RegKind, s: &str, ops: &dyn CatalogOps) -> Result<Reg, ExecError> {
    let trimmed = s.trim();
    // PG accepts the numeric spelling for every reg* type and does not check
    // that the OID exists — `999999::regclass` and `'999999'::regclass` both
    // render as the digits.
    if !trimmed.is_empty()
        && trimmed.bytes().all(|b| b.is_ascii_digit())
        && let Ok(oid) = trimmed.parse::<u32>()
    {
        return Ok(from_oid(kind, oid, ops));
    }
    // Ahead of the splitter, because `regtypein` reads its argument with the
    // *type-name grammar* and the splitter cannot stand in for it:
    // `character varying` is one type name, not two identifiers, and splitting
    // first rejects it before anything looks it up.
    //
    // TODO: a spelling the grammar rejects reports `invalid name syntax` here,
    // where PG reports the grammar's own error (`invalid type name ""`,
    // `unterminated quoted identifier at or near …`, `syntax error at end of
    // input`). Matching those means surfacing the type parser's errors.
    if kind == RegKind::Type
        && let Some(oid) = builtin_type_oid_from_syntax(trimmed)
    {
        return Ok(from_oid(kind, oid, ops));
    }
    let parts = split_qualified_name(trimmed).ok_or_else(invalid_name_syntax)?;
    let (namespace, name) = qualify(kind, &parts, || ops.current_database())?;
    // `regoperin` has a *third* answer the others do not: a name several
    // operators carry is an error rather than a miss.
    if kind == RegKind::Oper {
        return match ops.oper_oids(namespace.as_deref(), &name).as_slice() {
            [] => Err(not_found(kind, s, &parts)),
            [only] => Ok(from_oid(kind, *only, ops)),
            _ => Err(ExecError::new(
                sqlstate::AMBIGUOUS_FUNCTION,
                format!("more than one operator named {s}"),
            )),
        };
    }
    let oid = match kind {
        RegKind::Oper => unreachable!("returned above: an operator name is not one-or-nothing"),
        RegKind::Proc => ops.proc_oid(namespace.as_deref(), &name),
        RegKind::Class => ops.rel_oid(namespace.as_deref(), &name),
        RegKind::Type => builtin_type_oid(namespace.as_deref(), &name)
            .or_else(|| pseudo_type_oid(namespace.as_deref(), &name))
            .or_else(|| ops.user_type_oid(namespace.as_deref(), &name)),
        // `qualify` has already rejected a qualified schema name.
        RegKind::Namespace => ops.namespace_oid(&name),
    }
    .ok_or_else(|| not_found(kind, s, &parts))?;
    Ok(from_oid(kind, oid, ops))
}

/// The OID a type *spelling* denotes under PG's type-name grammar, which is how
/// `regtypein` resolves its argument — not by catalog lookup. Quoting is
/// therefore significant, and [`split_qualified_name`] has already discarded it
/// by the time [`builtin_type_oid`] runs: bare `char` is the `char(1)` keyword
/// (`bpchar`, oid 1042) while `"char"` is the one-byte type (oid 18). Running
/// the grammar on the raw input keeps the two apart, and also picks up the
/// spellings a bare catalog-name lookup misses, like `int4[]` and `varchar(10)`.
///
/// `None` for anything that is not a built-in spelling, so a user type still
/// falls through to the catalog.
fn builtin_type_oid_from_syntax(s: &str) -> Option<u32> {
    crabgresql_binder::builtin_type_from_syntax(s).map(|t| t.oid())
}

/// The OID of the built-in `namespace.name` names. Built-ins live in
/// `pg_catalog`, so any other qualifier names a user type instead. Both the
/// catalog and SQL spellings resolve (`'int4'::regtype` and
/// `'integer'::regtype` are the same value).
fn builtin_type_oid(namespace: Option<&str>, name: &str) -> Option<u32> {
    if matches!(namespace, Some(ns) if ns != "pg_catalog") {
        return None;
    }
    PgType::from_name(name).map(|t| t.oid())
}

/// The OID of a pseudo-type name (`'record'::regtype`). Pseudo-types live in
/// `pg_catalog` alongside the built-ins, so the same qualifier gate applies.
fn pseudo_type_oid(namespace: Option<&str>, name: &str) -> Option<u32> {
    if matches!(namespace, Some(ns) if ns != "pg_catalog") {
        return None;
    }
    crabgresql_types::pseudo_type_oid(name)
}

/// PG's "does not exist" error for a name that parsed but named nothing:
/// `relation "nosuchtable" does not exist`.
///
/// Which spelling of the name it echoes is per kind (probed against PG 18.4):
/// `regprocin` and `regoperin` pass their raw argument through, so
/// `'  NoSuch  '::regproc` reports `function "  NoSuch  "` with the spaces and
/// capitals intact, while the others report the *parsed* name —
/// `'PUB.NoSuch'::regclass` reports `relation "pub.nosuch"`.
fn not_found(kind: RegKind, raw: &str, parts: &[String]) -> ExecError {
    let state = match kind {
        RegKind::Proc | RegKind::Oper => sqlstate::UNDEFINED_FUNCTION,
        RegKind::Class => sqlstate::UNDEFINED_TABLE,
        RegKind::Type => sqlstate::UNDEFINED_OBJECT,
        RegKind::Namespace => sqlstate::INVALID_SCHEMA_NAME,
    };
    let message = match kind {
        RegKind::Oper => format!("operator does not exist: {raw}"),
        RegKind::Proc => format!("function \"{raw}\" does not exist"),
        _ => format!(
            "{} \"{}\" does not exist",
            kind.object_noun(),
            parts.join(".")
        ),
    };
    ExecError::new(state, message)
}

/// What every `reg*` input function raises for a name [`split_qualified_name`]
/// cannot take apart. A *syntax* error, not a miss: the string never named
/// anything to look up.
fn invalid_name_syntax() -> ExecError {
    ExecError::new(sqlstate::INVALID_NAME, "invalid name syntax")
}

/// Turn the parsed parts into the `(namespace, name)` the kind's lookup takes,
/// applying the rules PG's `DeconstructQualifiedName` applies — which is where
/// a name with too many parts stops being a miss and becomes an error.
///
/// Every message below was probed against PostgreSQL 18.4. Two of them are
/// worded per kind: `regclass` goes through `RangeVarGetRelidExtended`, which
/// quotes the whole dotted name and calls it a *relation* name, while the rest
/// go through `DeconstructQualifiedName`, which quotes nothing and calls it a
/// *qualified* name.
///
/// `current_database` is a thunk because only the three-part arm reads it, and
/// an ordinary `'pg_class'::regclass` should not pay for a database lookup.
fn qualify(
    kind: RegKind,
    parts: &[String],
    current_database: impl FnOnce() -> String,
) -> Result<(Option<String>, String), ExecError> {
    // A schema name is never itself qualified, and `regnamespacein` calls
    // anything else a syntax error rather than looking for a miss: `'a.b'` is
    // `invalid name syntax` here where for `regclass` it is a plain miss.
    if kind == RegKind::Namespace {
        return match parts {
            [name] => Ok((None, name.clone())),
            _ => Err(invalid_name_syntax()),
        };
    }
    let joined = || parts.join(".");
    match parts {
        [name] => Ok((None, name.clone())),
        [schema, name] => Ok((Some(schema.clone()), name.clone())),
        // The database part is simply dropped, so `'regression.public.t'`
        // resolves like `'public.t'` for a session connected to `regression`.
        [database, schema, name] if *database == current_database() => {
            Ok((Some(schema.clone()), name.clone()))
        }
        [_, _, _] => Err(ExecError::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            match kind {
                RegKind::Class => format!(
                    "cross-database references are not implemented: \"{}\"",
                    joined()
                ),
                _ => format!(
                    "cross-database references are not implemented: {}",
                    joined()
                ),
            },
        )),
        _ => Err(ExecError::new(
            sqlstate::SYNTAX_ERROR,
            match kind {
                RegKind::Class => format!(
                    "improper relation name (too many dotted names): {}",
                    joined()
                ),
                _ => format!(
                    "improper qualified name (too many dotted names): {}",
                    joined()
                ),
            },
        )),
    }
}

/// `regclass` input and `pg_get_viewdef(text)` reach the same
/// `makeRangeVarFromNameList`/`RangeVarGetRelid` pair upstream, so they share
/// this rather than each deciding what a malformed relation name means.
pub(crate) fn relation_name(
    s: &str,
    ops: &dyn CatalogOps,
) -> Result<(Option<String>, String), ExecError> {
    let parts = split_qualified_name(s.trim()).ok_or_else(invalid_name_syntax)?;
    qualify(RegKind::Class, &parts, || ops.current_database())
}

/// Split an object name into its dot-separated parts, applying SQL's identifier
/// rules the way PG's `SplitIdentifierString` does: an unquoted part folds to
/// lower case, a `"quoted"` part keeps its spelling (and `""` inside it is a
/// literal quote). How *many* parts are allowed is [`qualify`]'s to say, not
/// this function's.
///
/// `None` for a name that does not parse at all — an unterminated quote, an
/// empty unquoted part (`a.`, `.a`), trailing text after a closing quote
/// (`"a"x`), or a space inside an unquoted part (`a b`). An explicitly quoted
/// empty part is **not** malformed: `'""'::regclass` is a relation named `""`,
/// which merely does not exist.
pub(crate) fn split_qualified_name(s: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut rest = s;
    loop {
        let (part, tail) = take_ident(rest)?;
        parts.push(part);
        match tail.strip_prefix('.') {
            Some(next) => rest = next,
            None if tail.is_empty() => break,
            None => return None,
        }
    }
    Some(parts)
}

/// Take one identifier off the front of `s`, returning it and the remainder.
fn take_ident(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(body) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = body.char_indices();
        while let Some((i, c)) = chars.next() {
            if c != '"' {
                out.push(c);
                continue;
            }
            // `""` is an escaped quote; a lone `"` closes the identifier.
            match chars.clone().next() {
                Some((_, '"')) => {
                    out.push('"');
                    chars.next();
                }
                _ => {
                    // No emptiness check: a quoted empty part is legal, unlike
                    // an unquoted one — see [`split_qualified_name`].
                    let tail = &body[i + 1..];
                    return Some((out, tail.trim_start()));
                }
            }
        }
        // Ran off the end without a closing quote.
        return None;
    }
    let end = s
        .find(|c: char| c == '.' || c == '"' || c.is_whitespace())
        .unwrap_or(s.len());
    let (head, tail) = s.split_at(end);
    (!head.is_empty()).then(|| (head.to_ascii_lowercase(), tail.trim_start()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `regtypein` resolves with the type-name grammar, so quoting decides
    /// which of PG's two char types a spelling means. Resolving through
    /// `PgType::from_name` instead would make both spellings oid 18, because
    /// `split_qualified_name` has already dropped the quotes.
    #[test]
    fn regtype_distinguishes_quoted_char_from_the_keyword() {
        use crabgresql_types::oid;
        assert_eq!(builtin_type_oid_from_syntax("char"), Some(oid::BPCHAR));
        assert_eq!(builtin_type_oid_from_syntax("character"), Some(oid::BPCHAR));
        assert_eq!(builtin_type_oid_from_syntax("char(3)"), Some(oid::BPCHAR));
        assert_eq!(builtin_type_oid_from_syntax("\"char\""), Some(oid::CHAR));
        assert_eq!(
            builtin_type_oid_from_syntax("pg_catalog.char"),
            Some(oid::CHAR)
        );
        // Spellings a bare catalog-name lookup would miss.
        assert_eq!(
            builtin_type_oid_from_syntax("int4[]"),
            Some(oid::INT4_ARRAY)
        );
        assert_eq!(
            builtin_type_oid_from_syntax("varchar(10)"),
            Some(oid::VARCHAR)
        );
        // A user type is not a built-in and must fall through to the catalog.
        assert_eq!(builtin_type_oid_from_syntax("nosuchtype"), None);
    }

    /// The parts a name splits into, joined by `|` so an assertion reads as one
    /// string. No identifier below contains a `|`.
    fn split(s: &str) -> Option<String> {
        split_qualified_name(s).map(|parts| parts.join("|"))
    }

    #[test]
    fn unquoted_names_fold_and_quoted_names_do_not() {
        assert_eq!(split("PG_CLASS").as_deref(), Some("pg_class"));
        assert_eq!(split("  pg_class  ").as_deref(), Some("pg_class"));
        assert_eq!(split("rs.t").as_deref(), Some("rs|t"));
        assert_eq!(split("\"Mixed Case\"").as_deref(), Some("Mixed Case"));
        assert_eq!(split("\"RS\".\"T\"").as_deref(), Some("RS|T"));
        // An embedded `""` is one literal quote.
        assert_eq!(split("\"a\"\"b\"").as_deref(), Some("a\"b"));
        assert_eq!(split("a.b.c").as_deref(), Some("a|b|c"));
        // A quoted empty part is a name, not a malformation.
        assert_eq!(split("\"\"").as_deref(), Some(""));
        assert_eq!(split("rs.\"\"").as_deref(), Some("rs|"));
    }

    /// Everything here is `invalid name syntax` upstream — a string that never
    /// named anything, rather than a name that found nothing.
    #[test]
    fn malformed_names_are_rejected() {
        assert_eq!(split("\"unterminated"), None);
        assert_eq!(split(""), None);
        assert_eq!(split("   "), None);
        assert_eq!(split("a."), None);
        assert_eq!(split(".a"), None);
        assert_eq!(split("a..b"), None);
        assert_eq!(split("\"a\"x"), None);
        assert_eq!(split("a b"), None);
    }

    /// A three-part name is not automatically an error, and past three parts
    /// nothing saves it. Both wordings differ per kind.
    #[test]
    fn a_database_qualifier_resolves_only_for_the_connected_database() {
        let connected = || "regression".to_string();
        let q = |kind, s: &str| qualify(kind, &split_qualified_name(s).expect("parses"), connected);
        assert_eq!(
            q(RegKind::Class, "regression.public.t").expect("the connected database"),
            (Some("public".to_string()), "t".to_string())
        );
        let err = q(RegKind::Class, "nosuchdb.public.t").expect_err("another database");
        assert_eq!(err.code, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(
            err.message,
            "cross-database references are not implemented: \"nosuchdb.public.t\""
        );
        // Only `regclass` quotes the dotted name and calls it a relation name.
        let err = q(RegKind::Proc, "nosuchdb.public.f").expect_err("another database");
        assert_eq!(
            err.message,
            "cross-database references are not implemented: nosuchdb.public.f"
        );
        let err = q(RegKind::Class, "a.b.c.d").expect_err("four parts");
        assert_eq!(err.code, sqlstate::SYNTAX_ERROR);
        assert_eq!(
            err.message,
            "improper relation name (too many dotted names): a.b.c.d"
        );
        let err = q(RegKind::Type, "a.b.c.d").expect_err("four parts");
        assert_eq!(
            err.message,
            "improper qualified name (too many dotted names): a.b.c.d"
        );
        // A schema name is never qualified at all.
        let err = q(RegKind::Namespace, "a.b").expect_err("qualified schema");
        assert_eq!(err.code, sqlstate::INVALID_NAME);
        assert_eq!(err.message, "invalid name syntax");
    }

    #[test]
    fn builtin_types_resolve_under_both_spellings_and_only_in_pg_catalog() {
        let int4 = PgType::Int4.oid();
        assert_eq!(builtin_type_oid(None, "int4"), Some(int4));
        assert_eq!(builtin_type_oid(None, "integer"), Some(int4));
        assert_eq!(builtin_type_oid(Some("pg_catalog"), "int4"), Some(int4));
        // A user schema does not reach the built-in of the same spelling.
        assert_eq!(builtin_type_oid(Some("app"), "int4"), None);
    }
}
