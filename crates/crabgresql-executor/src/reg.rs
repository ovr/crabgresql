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
        // not reach. `regprocout` asks `FunctionIsVisible`, which an overloaded
        // built-in fails even from `pg_catalog`: PG prints oid 1740 as
        // `pg_catalog."numeric"` where this prints `"numeric"`.
        RegKind::Proc => ops.proc_name(oid).map(|name| quote_ident(&name)),
        // `regprocedure` names the same function by its whole signature, so an
        // overloaded name still reads back as the one function it came from.
        // The argument types print in their SQL spelling and with no space
        // after the comma — `format_procedure` runs `format_type_be` over each,
        // which is what `format_type_text` is.
        //
        // TODO: schema-qualify an unreachable name, as for `regproc` above.
        RegKind::Procedure => {
            let (_namespace, name, args) = ops.proc_signature(oid)?;
            let args = args
                .iter()
                .map(|arg| crate::eval::format_type_text(*arg, None, Some(ops)))
                .collect::<Vec<_>>()
                .join(",");
            Some(format!("{}({args})", quote_ident(&name)))
        }
        // An operator prints bare only when its bare name would read *back* as
        // this same operator — the round trip `regoperin` would make. `=` names
        // some ninety operators, so most of them print schema-qualified even
        // though `pg_catalog` is always on the search path.
        RegKind::Oper => {
            let operator = ops.oper_signature(oid)?;
            Some(match ops.oper_oids(None, &operator.name).as_slice() {
                [only] if *only == oid => operator.name,
                // An operator name is punctuation, never an identifier, so only
                // the schema is quoted: `pg_catalog.+`.
                _ => format!("{}.{}", quote_ident(&operator.namespace), operator.name),
            })
        }
        // The operand types are what make the name unambiguous, so unlike
        // `regoper` this qualifies by *visibility* (PG's `OperatorIsVisible`)
        // rather than by how many operators share the name — probed with a
        // `CREATE OPERATOR rcx.###`, which prints qualified only while `rcx` is
        // off the search path. Every operator this build publishes is a
        // `pg_catalog` built-in, so the name is always bare.
        //
        // TODO: qualify by visibility once `CREATE OPERATOR` exists and an
        // operator can live off the search path.
        RegKind::Operator => {
            let operator = ops.oper_signature(oid)?;
            Some(format!(
                "{}({},{})",
                operator.name,
                operand_name(operator.left, ops),
                operand_name(operator.right, ops),
            ))
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
        RegKind::Type => type_name(oid, ops),
        RegKind::Namespace => ops.namespace_name(oid).map(|n| quote_ident(&n)),
    }
}

/// How a type OID prints, which is `format_type_be` upstream: a built-in under
/// its SQL spelling, not its catalog one — 23 is `integer`, 1005 is
/// `smallint[]`, 1043 is `character varying`. That is exactly `PgType::name`.
///
/// `regtype` output and the operand types `regoperator` prints share this,
/// because upstream both reach `format_type_be`.
fn type_name(oid: u32, ops: &dyn CatalogOps) -> Option<String> {
    match PgType::from_oid(oid) {
        Some(ty) => Some(ty.name().to_string()),
        // A pseudo-type has a catalog row but no `PgType`, so it names itself
        // from the shared table — `pg_typeof` reports `unknown` for an untyped
        // literal, and `record`/`void`/`anyelement` turn up in introspection.
        // Ahead of the user-type lookup: these OIDs are all below the
        // user-OID floor, so the order is belt-and-braces.
        None => crabgresql_types::pseudo_type_name(oid)
            .map(str::to_string)
            .or_else(|| ops.user_type_name(oid).map(|(_, name)| quote_ident(&name))),
    }
}

/// One operand of a `regoperator` name. A prefix operator has no left operand,
/// which the catalog stores as 0 and PG prints as `NONE` — the same spelling its
/// input function reads back.
///
/// The `???` is `format_type_be`'s rendering of a type OID it cannot find, and
/// differs from `regtype`'s (`999999::regtype` is the digits). Unreachable
/// through a catalog row, since an operator's operand types exist by
/// construction.
fn operand_name(oid: u32, ops: &dyn CatalogOps) -> String {
    match oid {
        0 => "NONE".to_string(),
        oid => type_name(oid, ops).unwrap_or_else(|| "???".to_string()),
    }
}

/// Resolve a `reg*` input string to a value, as PG's `regclassin` and friends
/// do. All digits is an OID written directly; anything else is an object name,
/// optionally schema-qualified and optionally quoted.
pub fn from_text(kind: RegKind, s: &str, ops: &dyn CatalogOps) -> Result<Reg, ExecError> {
    // The two shortcuts every `reg*` input function takes before it parses
    // anything, and both read the argument **as written**: PG compares the raw
    // string, so `'  1259  '::regclass` is a relation named `1259` rather than
    // an OID, and `'-'` is `InvalidOid` only when it stands alone.
    //
    // Neither operator kind takes the dash: `-` is a legal operator name, so
    // `regoper` looks it up like any other name and `regoperator` wants a
    // signature (`'-'::regoperator` is `expected a left parenthesis`).
    if s == "-" && !matches!(kind, RegKind::Oper | RegKind::Operator) {
        return Ok(from_oid(kind, 0, ops));
    }
    // PG accepts the numeric spelling for every reg* type and does not check
    // that the OID exists — `999999::regclass` and `'999999'::regclass` both
    // render as the digits.
    if !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_digit())
        && let Ok(oid) = s.parse::<u32>()
    {
        return Ok(from_oid(kind, oid, ops));
    }
    let trimmed = s.trim();
    // `regtypein` reads its whole argument as a type name, which is not the
    // splitter's grammar — see [`resolve_type_name`].
    if kind == RegKind::Type {
        return Ok(from_oid(kind, resolve_type_name(trimmed, ops)?, ops));
    }
    // `regprocedurein` reads a *signature* too, `abs(numeric)`, and shares the
    // signature grammar with `regoperatorin` below.
    if kind == RegKind::Procedure {
        return procedure_from_signature(s, trimmed, ops);
    }
    // `regoperatorin` reads a *signature*, `+(integer,integer)`, so the whole
    // argument is not a name either: the splitter only ever sees the part ahead
    // of the parenthesis.
    if kind == RegKind::Operator {
        return operator_from_signature(s, trimmed, ops);
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
        RegKind::Oper | RegKind::Operator | RegKind::Procedure | RegKind::Type => {
            unreachable!("returned above: none of these is looked up by a bare name here")
        }
        RegKind::Proc => ops.proc_oid(namespace.as_deref(), &name),
        RegKind::Class => ops.rel_oid(namespace.as_deref(), &name),
        // `qualify` has already rejected a qualified schema name.
        RegKind::Namespace => ops.namespace_oid(&name),
    }
    .ok_or_else(|| not_found(kind, s, &parts))?;
    Ok(from_oid(kind, oid, ops))
}

/// The relation a *name* denotes, for the privilege functions — PG's
/// `RangeVarGetRelid` over `textToQualifiedNameList`, which is **not**
/// `regclassin`.
///
/// The difference is the two shortcuts [`from_text`] takes first: there, all
/// digits are an OID written directly and `-` is `InvalidOid`. Here they are
/// ordinary names, so `has_table_privilege('1259','SELECT')` reports
/// `relation "1259" does not exist` where `'1259'::regclass` is `pg_class`.
/// Everything past the shortcuts — case folding, quoting, qualification — is
/// the same grammar, so it is shared rather than restated.
pub fn relation_oid_from_name(s: &str, ops: &dyn CatalogOps) -> Result<u32, ExecError> {
    let parts = split_qualified_name(s.trim()).ok_or_else(invalid_name_syntax)?;
    let (namespace, name) = qualify(RegKind::Class, &parts, || ops.current_database())?;
    // `RangeVarGetRelid` resolves the namespace first, so `'nosuch.t'` is a
    // missing *schema* rather than a missing relation — one more place it and
    // `regclassin` part company. `information_schema` lands here as well: this
    // build serves three of its views but publishes no `pg_namespace` row for
    // the schema, so every privilege question about it reports that one fact.
    if let Some(namespace) = &namespace
        && ops.namespace_oid(namespace).is_none()
    {
        return Err(ExecError::new(
            sqlstate::INVALID_SCHEMA_NAME,
            format!("schema \"{namespace}\" does not exist"),
        ));
    }
    ops.rel_oid(namespace.as_deref(), &name)
        // Belt and braces: 0 is `InvalidOid`, never an object to ask about.
        .filter(|oid| *oid != 0)
        .ok_or_else(|| not_found(RegKind::Class, s, &parts))
}

/// The OID a type *name* denotes, which is PG's `parseTypeString` — what
/// `regtypein` resolves its whole argument with, and what `regoperatorin`
/// resolves each operand with.
///
/// Not the splitter's grammar, and the splitter cannot stand in for it:
/// `character varying` is one type name rather than two identifiers, and
/// `varchar(10)` and `int4[]` are names a bare catalog lookup never sees.
/// Quoting matters here too, for the reason [`builtin_type_oid_from_syntax`]
/// gives.
///
/// The numeric spelling is *not* accepted here: `'23'` is a type named `23`,
/// which does not exist. Only [`from_text`] reads digits as an OID, and it does
/// so before calling this.
///
/// A spelling the grammar cannot read at all is the grammar's own error —
/// `syntax error at or near "int4"` for `'int4 int4'` — reported under the
/// `invalid type name "<spec>"` context PG adds while parsing a type out of a
/// string. A name it reads but nothing answers to is the ordinary
/// `type "x" does not exist`, and a qualified one whose schema is missing
/// reports *that* first: `'ng_catalog.int4'::regtype` is a missing schema.
///
/// TODO: a spelling *this* grammar accepts and PG's does not still resolves as
/// a name — a bare reserved word (`'select'`) reports `type "select" does not
/// exist` where PG reports a syntax error. See
/// [`crabgresql_parser::parse_data_type`], which documents why a keyword list
/// cannot close it.
fn resolve_type_name(s: &str, ops: &dyn CatalogOps) -> Result<u32, ExecError> {
    if let Err(e) = crabgresql_parser::parse_data_type(s) {
        return Err(ExecError::new(e.sqlstate, e.message)
            .push_context(format!("invalid type name \"{s}\"")));
    }
    if let Some(oid) = builtin_type_oid_from_syntax(s) {
        return Ok(oid);
    }
    let parts = split_qualified_name(s).ok_or_else(invalid_name_syntax)?;
    let (namespace, name) = qualify(RegKind::Type, &parts, || ops.current_database())?;
    if let Some(namespace) = &namespace
        && ops.namespace_oid(namespace).is_none()
    {
        return Err(ExecError::new(
            sqlstate::INVALID_SCHEMA_NAME,
            format!("schema \"{namespace}\" does not exist"),
        ));
    }
    builtin_type_oid(namespace.as_deref(), &name)
        .or_else(|| pseudo_type_oid(namespace.as_deref(), &name))
        .or_else(|| ops.user_type_oid(namespace.as_deref(), &name))
        .ok_or_else(|| not_found(RegKind::Type, s, &parts))
}

/// How many arguments a signature may carry — PostgreSQL's `FUNC_MAX_ARGS`,
/// which is a compile-time constant upstream and 100 in every stock build.
const MAX_ARGS: usize = 100;

/// `regprocedurein`: resolve `name(argtype,…)` to the one function that has
/// that signature, which is what makes an overloaded name resolvable where
/// `regproc` refuses it. `raw` is the argument exactly as written, which the
/// "does not exist" error echoes; `trimmed` is what parses.
///
/// The steps run in the order PG's `parseNameAndArgTypes` and `LookupFuncName`
/// run them, which is observable: `'a.b.c.d(nosuchtype)'` reports the *type*
/// and only `'a.b.c.d(int4)'` gets as far as the four-part name, and a bad type
/// beats a too-long argument list. All probed against PG 18.4.
fn procedure_from_signature(
    raw: &str,
    trimmed: &str,
    ops: &dyn CatalogOps,
) -> Result<Reg, ExecError> {
    let (parts, args) = split_name_and_arg_types(trimmed)?;
    let arg_types = args
        .iter()
        .map(|arg| signature_arg_oid(arg, ops))
        .collect::<Result<Vec<_>, _>>()?;
    if arg_types.len() > MAX_ARGS {
        // A limit, not a malformation, so PG codes it 54023 where every other
        // complaint about this list is 22P02. `regoperator`'s version of this
        // message carries a HINT and this one does not (probed through
        // `pg_input_error_info`).
        return Err(ExecError::new(
            sqlstate::TOO_MANY_ARGUMENTS,
            "too many arguments",
        ));
    }
    let (namespace, name) = qualify(RegKind::Procedure, &parts, || ops.current_database())?;
    let oid = ops
        .proc_oid_by_signature(namespace.as_deref(), &name, &arg_types)
        .ok_or_else(|| not_found(RegKind::Procedure, raw, &parts))?;
    Ok(from_oid(RegKind::Procedure, oid, ops))
}

/// One argument of a signature. Unlike [`operand_oid`] there is no `NONE`:
/// every argument of a function is a type, and `'int4pl(none,int4)'` is a
/// syntax error upstream rather than an absent argument.
fn signature_arg_oid(arg: &str, ops: &dyn CatalogOps) -> Result<u32, ExecError> {
    if arg.is_empty() {
        // The type parser's own error for the empty string, which is what an
        // argument list like `(,int4)` hands it.
        return Err(ExecError::new(
            sqlstate::SYNTAX_ERROR,
            "invalid type name \"\"",
        ));
    }
    resolve_type_name(arg, ops)
}

/// `regoperatorin`: resolve `name(lefttype,righttype)` to the operator it names.
/// `raw` is the argument exactly as written, which the "does not exist" error
/// echoes; `trimmed` is what parses.
///
/// The order the steps run in is observable, and was probed against PG 18.4:
/// `'a.b.c.d.+(nosuchtype,int4)'` reports the *type*, `'a.b.c.d.+(int4)'`
/// reports the missing argument, and only `'a.b.c.d.+(int4,int4)'` gets as far
/// as complaining about the dotted name.
fn operator_from_signature(
    raw: &str,
    trimmed: &str,
    ops: &dyn CatalogOps,
) -> Result<Reg, ExecError> {
    let (parts, args) = split_name_and_arg_types(trimmed)?;
    let operands = args
        .iter()
        .map(|arg| operand_oid(arg, ops))
        .collect::<Result<Vec<_>, _>>()?;
    let [left, right] = operands[..] else {
        // Not a syntax error: the signature parsed, it just named the wrong
        // number of operands. PG counts zero operands as too many, not as too
        // few — `'+()'::regoperator` is "too many arguments".
        return Err(match operands.len() {
            1 => ExecError::new(sqlstate::UNDEFINED_PARAMETER, "missing argument").with_hint(Some(
                "Use NONE to denote the missing argument of a unary operator.".to_string(),
            )),
            _ => ExecError::new(sqlstate::TOO_MANY_ARGUMENTS, "too many arguments")
                .with_hint(Some("Provide two argument types for operator.".to_string())),
        });
    };
    let (namespace, name) = qualify(RegKind::Operator, &parts, || ops.current_database())?;
    let oid = ops
        .oper_oid(namespace.as_deref(), &name, left, right)
        .ok_or_else(|| not_found(RegKind::Operator, raw, &parts))?;
    Ok(from_oid(RegKind::Operator, oid, ops))
}

/// One operand of a signature: a type name, or `NONE` for the operand a prefix
/// operator does not have. The `NONE` test is case-insensitive and reads the
/// argument as written, so a quoted `"NONE"` is a type name — probed:
/// `'=(none,int4)'` is a missing operator, `'=("NONE",int4)'` is a missing
/// *type*.
fn operand_oid(arg: &str, ops: &dyn CatalogOps) -> Result<u32, ExecError> {
    if arg.eq_ignore_ascii_case("none") {
        return Ok(0);
    }
    signature_arg_oid(arg, ops)
}

/// Split `name(type,type)` into the name's dot-separated parts and the operand
/// type names as written, the way PG's `parseNameAndArgTypes` does.
///
/// Every error below is one PG raises before it looks anything up, and each was
/// probed against PG 18.4 (SQLSTATEs included: the parenthesis and type-name
/// complaints are `22P02`, a name that does not split is `42602`).
fn split_name_and_arg_types(s: &str) -> Result<(Vec<String>, Vec<&str>), ExecError> {
    let Some(open) = find_unquoted(s, '(') else {
        return Err(ExecError::new(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "expected a left parenthesis",
        ));
    };
    let parts = split_qualified_name(s[..open].trim()).ok_or_else(invalid_name_syntax)?;
    // The closing parenthesis is the last non-space character, so trailing text
    // (`'+(int4,int4) x'`) is a missing right parenthesis rather than junk.
    let rest = s[open + 1..].trim_end();
    let Some(inner) = rest.strip_suffix(')') else {
        return Err(ExecError::new(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "expected a right parenthesis",
        ));
    };
    Ok((parts, split_arg_types(inner)?))
}

/// The argument list between the parentheses, cut at the commas that are
/// outside quotes and outside any bracketing — a typmod and an array bound both
/// carry commas of their own, so `'=(numeric(10,2),numeric)'` is two operands
/// and `'int4pl(int4[1,2])'` is one argument (probed: PG's `CONTEXT` names the
/// whole `int4[1,2]`, and then the type grammar rejects it).
///
/// Bracketing is a **matched stack**, and an unmatched closer or a leftover
/// opener is `improper type name` — the one complaint about this list that does
/// not come from the type grammar, and the reason a bare `'numeric)'::regtype`
/// reports a syntax error while `'int4pl(numeric))'` does not.
fn split_arg_types(inner: &str) -> Result<Vec<&str>, ExecError> {
    let improper = || ExecError::new(sqlstate::INVALID_TEXT_REPRESENTATION, "improper type name");
    let mut args = Vec::new();
    let mut start = 0;
    let mut open = Vec::new();
    let mut in_quote = false;
    for (i, c) in inner.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            _ if in_quote => {}
            '(' | '[' => open.push(c),
            // A closer with nothing (or the wrong opener) under it: the extra
            // parenthesis in `'+(int4,int4))'`, or the `]` in `'int4pl(int4])'`.
            ')' => {
                if open.pop() != Some('(') {
                    return Err(improper());
                }
            }
            ']' => {
                if open.pop() != Some('[') {
                    return Err(improper());
                }
            }
            ',' if open.is_empty() => {
                args.push(inner[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if in_quote || !open.is_empty() {
        return Err(improper());
    }
    let last = inner[start..].trim();
    match (last.is_empty(), args.is_empty()) {
        // Nothing at all between the parentheses: no operands.
        (true, true) => {}
        // A trailing comma, so an operand was promised and not written. An
        // *empty* one between two commas is a different error, raised by the
        // type parser — see [`operand_oid`].
        (true, false) => {
            return Err(ExecError::new(
                sqlstate::INVALID_TEXT_REPRESENTATION,
                "expected a type name",
            ));
        }
        (false, _) => args.push(last),
    }
    Ok(args)
}

/// The byte offset of the first `needle` outside a quoted section. Toggling on
/// every `"` is what handles the doubled `""` [`take_ident`] reads as one
/// literal quote: it closes the section and reopens it, leaving `needle` inside
/// it either way.
fn find_unquoted(s: &str, needle: char) -> Option<usize> {
    let mut in_quote = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            c if c == needle && !in_quote => return Some(i),
            _ => {}
        }
    }
    None
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
        RegKind::Proc | RegKind::Procedure | RegKind::Oper | RegKind::Operator => {
            sqlstate::UNDEFINED_FUNCTION
        }
        RegKind::Class => sqlstate::UNDEFINED_TABLE,
        RegKind::Type => sqlstate::UNDEFINED_OBJECT,
        RegKind::Namespace => sqlstate::INVALID_SCHEMA_NAME,
    };
    let message = match kind {
        RegKind::Oper | RegKind::Operator => format!("operator does not exist: {raw}"),
        RegKind::Proc | RegKind::Procedure => format!("function \"{raw}\" does not exist"),
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

/// The relation a function argument names, or the error PostgreSQL raises for
/// it — what `pg_get_viewdef(text)` and `pg_get_serial_sequence(text, text)`
/// both take.
///
/// Three outcomes, each probed against PostgreSQL 18.4:
///
/// ```text
/// pg_get_serial_sequence('123','id')         42P01 relation "123" does not exist
/// pg_get_serial_sequence('PUB.NoSuch','id')  3F000 schema "pub" does not exist
/// pg_get_serial_sequence('public.NoSuch',…)  42P01 relation "public.nosuch" does not exist
/// ```
///
/// A missing *schema* is reported before the relation, and both messages quote
/// the parsed spelling rather than the argument as written. Unlike
/// [`from_text`], all digits is a relation named `123` rather than an OID: these
/// functions take a name, and only a `reg*` cast takes the numeric form.
///
/// Returns the relation's OID together with the parsed name, since a caller
/// that looks the relation up again should look it up under the spelling that
/// resolved.
pub(crate) fn resolve_relation(
    s: &str,
    ops: &dyn CatalogOps,
) -> Result<(u32, Option<String>, String), ExecError> {
    let (namespace, name) = relation_name(s, ops)?;
    if let Some(namespace) = &namespace
        && ops.namespace_oid(namespace).is_none()
    {
        return Err(ExecError::new(
            sqlstate::INVALID_SCHEMA_NAME,
            format!("schema \"{namespace}\" does not exist"),
        ));
    }
    let oid = ops.rel_oid(namespace.as_deref(), &name).ok_or_else(|| {
        let qualified = match &namespace {
            Some(namespace) => format!("{namespace}.{name}"),
            None => name.clone(),
        };
        ExecError::new(
            sqlstate::UNDEFINED_TABLE,
            format!("relation \"{qualified}\" does not exist"),
        )
    })?;
    Ok((oid, namespace, name))
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

    /// The signature a `regoperator` argument parses into, as `name|name(arg,arg)`
    /// so an assertion reads as one string.
    fn signature(s: &str) -> Result<String, ExecError> {
        let (parts, args) = split_name_and_arg_types(s)?;
        Ok(format!("{}({})", parts.join("|"), args.join(",")))
    }

    /// [`signature`] for a spelling that must parse.
    fn sig(s: &str) -> String {
        signature(s).unwrap_or_else(|e| panic!("{s:?} should parse: {}", e.message))
    }

    /// What `parseNameAndArgTypes` accepts. Every spelling here was probed
    /// against PG 18.4 and resolves to the same operator.
    #[test]
    fn a_signature_splits_into_a_name_and_its_operands() {
        assert_eq!(sig("+(int4,int4)"), "+(int4,int4)");
        assert_eq!(sig(" + ( int4 , int4 ) "), "+(int4,int4)");
        assert_eq!(sig("\"+\"(int4,int4)"), "+(int4,int4)");
        assert_eq!(sig("PG_CATALOG.+(int4,int4)"), "pg_catalog|+(int4,int4)");
        // A typmod's comma is inside parentheses, so it does not cut an operand
        // in two.
        assert_eq!(sig("=(numeric(10,2),numeric)"), "=(numeric(10,2),numeric)");
        // A type name is not one identifier either.
        assert_eq!(
            sig("=(character varying,text)"),
            "=(character varying,text)"
        );
        // No operands at all is zero of them, which the caller reports as too
        // many — not one empty operand.
        assert_eq!(sig("+()"), "+()");
        assert_eq!(sig("+(  )"), "+()");
        // One operand parses; only the count check rejects it.
        assert_eq!(sig("+(int4)"), "+(int4)");
    }

    /// Everything a `regoperator` argument is rejected for before anything is
    /// looked up, with the SQLSTATE PG raises (probed with `VERBOSITY verbose`).
    #[test]
    fn a_malformed_signature_is_rejected_before_any_lookup() {
        let err = |s: &str| {
            let e = signature(s).expect_err("malformed");
            format!("{} {}", e.code, e.message)
        };
        assert_eq!(err("+"), "22P02 expected a left parenthesis");
        assert_eq!(err(""), "22P02 expected a left parenthesis");
        // The name is what has to be a name; the operands are type syntax.
        assert_eq!(err("(int4,int4)"), "42602 invalid name syntax");
        assert_eq!(err("+(int4,int4"), "22P02 expected a right parenthesis");
        // Trailing text reads as a *missing* parenthesis rather than as junk.
        assert_eq!(err("+(int4,int4) x"), "22P02 expected a right parenthesis");
        assert_eq!(err("=(int4,)"), "22P02 expected a type name");
        assert_eq!(err("+(int4,int4))"), "22P02 improper type name");
        // A typmod's own parenthesis can close the list, and then what is left
        // is unbalanced: `'=(numeric(10,int4)'` is an improper type name rather
        // than a missing parenthesis, because the `)` it does have was taken as
        // the closing one.
        assert_eq!(err("=(numeric(10,int4)"), "22P02 improper type name");
        assert_eq!(err("=(numeric(10,2,int4)"), "22P02 improper type name");
    }

    /// Square brackets bracket the list the same way parentheses do, and must
    /// match by kind: an array bound hides its comma (`'int4pl(int4[1,2])'` is
    /// one argument, which the type grammar then rejects — PG's `CONTEXT` names
    /// the whole spelling), while a stray or mismatched bracket is `improper
    /// type name` before anything parses.
    #[test]
    fn brackets_bracket_the_argument_list_and_must_match() {
        assert_eq!(sig("int4pl(int4[1,2])"), "int4pl(int4[1,2])");
        assert_eq!(sig("int4pl(int4[],int4)"), "int4pl(int4[],int4)");
        let err = |s: &str| {
            let e = signature(s).expect_err("the brackets do not match");
            format!("{} {}", e.code, e.message)
        };
        assert_eq!(err("int4pl(int4])"), "22P02 improper type name");
        assert_eq!(err("int4pl(int4[)"), "22P02 improper type name");
        assert_eq!(err("int4pl([int4)"), "22P02 improper type name");
    }

    /// An operand written between two commas is *empty*, not absent: the
    /// splitter hands it on, and the type parser is what rejects it with a
    /// different error than the trailing comma above. What the operand
    /// spellings mean — `NONE`, a quoted `"NONE"`, a user type — needs a
    /// catalog, so `regcast` covers that.
    #[test]
    fn an_empty_operand_reaches_the_type_parser() {
        let (_, args) = split_name_and_arg_types("=(,int4)").expect("parses");
        assert_eq!(args, ["", "int4"]);
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
