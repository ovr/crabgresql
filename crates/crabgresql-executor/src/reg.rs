//! `reg*` input and output: turning an object name into an OID and back.
//!
//! PostgreSQL keeps only the OID in a `reg*` value and resolves the name in the
//! type's output function. `Value::encode_text` here is pure, so resolution
//! happens when the value is *built* — in this module — and the rendered name
//! travels with it (see [`crabgresql_types::Reg`]).
//!
//! Every rendering below was probed against PostgreSQL 18.4.

use crabgresql_types::{PgType, Reg, RegKind, text::quote_ident};
use crabgresql_pg_wire::sqlstate;

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
            None => ops.user_type_name(oid).map(|(_, name)| quote_ident(&name)),
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
    if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(oid) = trimmed.parse::<u32>() {
            return Ok(from_oid(kind, oid, ops));
        }
    }
    let (namespace, name) = split_qualified_name(trimmed).ok_or_else(|| not_found(kind, s))?;
    let oid = match kind {
        RegKind::Class => ops.rel_oid(namespace.as_deref(), &name),
        RegKind::Type => builtin_type_oid(namespace.as_deref(), &name)
            .or_else(|| ops.user_type_oid(namespace.as_deref(), &name)),
        // A schema name is never itself qualified.
        RegKind::Namespace => match namespace {
            Some(_) => None,
            None => ops.namespace_oid(&name),
        },
    }
    .ok_or_else(|| not_found(kind, s))?;
    Ok(from_oid(kind, oid, ops))
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

/// PG's "does not exist" error for a name that resolved to nothing, quoting the
/// input as written: `relation "nosuchtable" does not exist`.
fn not_found(kind: RegKind, s: &str) -> ExecError {
    let state = match kind {
        RegKind::Class => sqlstate::UNDEFINED_TABLE,
        RegKind::Type => sqlstate::UNDEFINED_OBJECT,
        RegKind::Namespace => sqlstate::INVALID_SCHEMA_NAME,
    };
    ExecError::new(
        state,
        format!("{} \"{}\" does not exist", kind.object_noun(), s.trim()),
    )
}

/// Split an object name into an optional schema and a name, applying SQL's
/// identifier rules: an unquoted part folds to lower case, a `"quoted"` part
/// keeps its spelling (and `""` inside it is a literal quote). `None` for a
/// malformed name — an unterminated quote, an empty part, or more than two
/// parts (no `db.schema.table` here, since there is one database).
fn split_qualified_name(s: &str) -> Option<(Option<String>, String)> {
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
    match parts.as_slice() {
        [name] => Some((None, name.clone())),
        [schema, name] => Some((Some(schema.clone()), name.clone())),
        _ => None,
    }
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
                    let tail = &body[i + 1..];
                    return (!out.is_empty()).then_some((out, tail.trim_start()));
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

    #[test]
    fn unquoted_names_fold_and_quoted_names_do_not() {
        assert_eq!(
            split_qualified_name("PG_CLASS"),
            Some((None, "pg_class".to_string()))
        );
        assert_eq!(
            split_qualified_name("  pg_class  "),
            Some((None, "pg_class".to_string()))
        );
        assert_eq!(
            split_qualified_name("rs.t"),
            Some((Some("rs".to_string()), "t".to_string()))
        );
        assert_eq!(
            split_qualified_name("\"Mixed Case\""),
            Some((None, "Mixed Case".to_string()))
        );
        assert_eq!(
            split_qualified_name("\"RS\".\"T\""),
            Some((Some("RS".to_string()), "T".to_string()))
        );
        // An embedded `""` is one literal quote.
        assert_eq!(
            split_qualified_name("\"a\"\"b\""),
            Some((None, "a\"b".to_string()))
        );
    }

    #[test]
    fn malformed_names_are_rejected() {
        // Unterminated quote, empty parts, and too many parts all fail rather
        // than resolving to something surprising.
        assert_eq!(split_qualified_name("\"unterminated"), None);
        assert_eq!(split_qualified_name(""), None);
        assert_eq!(split_qualified_name("a."), None);
        assert_eq!(split_qualified_name(".a"), None);
        assert_eq!(split_qualified_name("a.b.c"), None);
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
