//! `pg_proc`: the referenced built-in functions plus this server's routines.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;
use std::collections::HashMap;

use crabgresql_types::{Reg, RegKind};

use crate::{CatalogRoutine, PG_PROC_ROWS};

/// `pg_catalog.pg_proc` — the columns clients read, in PostgreSQL's order.
pub(crate) fn pg_proc_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_proc",
        "pg_catalog",
        vec![
            col("oid", PgType::Oid),
            col("proname", PgType::Name),
            col("pronamespace", PgType::Oid),
            col("proowner", PgType::Oid),
            col("prolang", PgType::Oid),
            col("procost", PgType::Float4),
            col("prorows", PgType::Float4),
            col("provariadic", PgType::Oid),
            col("prosupport", REGPROC),
            col("prokind", CHARLIKE),
            col("prosecdef", PgType::Bool),
            col("proleakproof", PgType::Bool),
            col("proisstrict", PgType::Bool),
            col("proretset", PgType::Bool),
            col("provolatile", CHARLIKE),
            col("proparallel", CHARLIKE),
            col("pronargs", PgType::Int2),
            col("pronargdefaults", PgType::Int2),
            col("prorettype", PgType::Oid),
            col("proargtypes", OIDVECTOR),
            col("proallargtypes", PgType::Array(crabgresql_types::oid::OID)),
            col("proargmodes", PgType::Array(crabgresql_types::oid::TEXT)),
            col("proargnames", PgType::Array(crabgresql_types::oid::TEXT)),
            col("proargdefaults", NODE_TREE),
            col("protrftypes", PgType::Array(crabgresql_types::oid::OID)),
            col("prosrc", PgType::Text),
            col("probin", PgType::Text),
            col("prosqlbody", NODE_TREE),
            col("proconfig", PgType::Array(crabgresql_types::oid::TEXT)),
            col("proacl", ACLITEM_ARRAY),
        ],
    )
}

/// The built-in `pg_proc` rows generated from `pg_proc.dat` — the functions the
/// other catalogs reference, and only those (see `crabgresql-bki`'s `pg_proc`
/// module).
/// Callers append the session's `CREATE FUNCTION` routines after these.
///
/// `proallargtypes`/`proargmodes`/`proargnames` are filled for the few that
/// declare OUT or VARIADIC parameters (`json_extract_path`, the ordered-set
/// aggregate support functions) and NULL for the rest, which is what
/// PostgreSQL stores. `pronargdefaults` is 0 for every one: codegen refuses an
/// entry carrying an argument default, because nothing here can render the
/// expression back.
///
/// The four columns that are NULL for every row here and in [`user_rows`] are
/// NULL because there is nothing to record, not as a placeholder:
///
/// * `proargdefaults` — no routine declares an argument default, which is the
///   same fact `pronargdefaults = 0` states. Upstream's `opr_sanity` checks
///   exactly that the two agree ("pronargdefaults should be 0 iff
///   proargdefaults is null").
/// * `protrftypes` — there are no transforms to name.
/// * `prosqlbody` — a routine written in the standard `RETURN`/`BEGIN ATOMIC`
///   form does have one, and `pg_get_function_sqlbody` renders it (which is
///   what `pg_dump` reads), but the *column* stays NULL: PostgreSQL stores a
///   `pg_node_tree` here, and nothing in this build renders a node tree. The
///   NULL is therefore a gap, not a statement that no body exists — the one
///   place in this list where the two differ. `prosrc` is empty for such a
///   routine, exactly as in PostgreSQL.
/// * `proconfig` — `CREATE FUNCTION` parses no `SET` clause, so no routine has
///   a per-call GUC setting.
pub(crate) fn pg_proc_builtin_rows() -> Vec<Vec<Value>> {
    // crabgresql's own table-AM handlers, so `pg_am.amhandler` resolves for
    // every method this build ships rather than only the upstream ones. They
    // are shaped like PostgreSQL's: volatile, strict, `internal` argument,
    // returning `table_am_handler` (oid 269).
    let own = OWN_AM_HANDLERS.iter().map(|(oid, name)| {
        vec![
            Value::Oid(*oid),
            Value::Text((*name).to_string()),
            Value::Oid(11),
            Value::Oid(BOOTSTRAP_ROLE_OID),
            Value::Oid(12),
            Value::Float4(1.0),
            Value::Float4(0.0),
            Value::Oid(0),
            Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
            chr('f'),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            chr('v'),
            chr('s'),
            Value::Int2(1),
            Value::Int2(0),
            Value::Oid(269),
            oidvector([2281]),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Text((*name).to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]
    });
    // The snowball dictionary's two C functions, which `pg_ts_template.snowball`
    // points at. `initdb` creates them from `snowball_create.sql` rather than
    // from a `.dat`, so codegen has nothing to emit and they are spelled out
    // here — shaped as PostgreSQL's are: language `c`, volatile, strict,
    // `internal` arguments and result, living in `$libdir/dict_snowball`.
    let snowball = [(SNOWBALL_INIT_PROC, 1), (SNOWBALL_LEXIZE_PROC, 4)]
        .into_iter()
        .map(|(proc, nargs)| {
            vec![
                Value::Oid(proc.oid),
                Value::Text(proc.name.to_string()),
                Value::Oid(11),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(13),
                Value::Float4(1.0),
                Value::Float4(0.0),
                Value::Oid(0),
                Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
                chr('f'),
                Value::Bool(false),
                Value::Bool(false),
                Value::Bool(true),
                Value::Bool(false),
                chr('v'),
                chr('u'),
                Value::Int2(nargs),
                Value::Int2(0),
                Value::Oid(2281),
                oidvector(std::iter::repeat_n(2281, nargs as usize)),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Text(proc.name.to_string()),
                Value::Text("$libdir/dict_snowball".to_string()),
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        });
    PG_PROC_ROWS
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.proname.to_string()),
                // Every built-in lives in `pg_catalog`, owned by the bootstrap
                // superuser.
                Value::Oid(11),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(r.prolang),
                Value::Float4(r.procost),
                Value::Float4(r.prorows),
                Value::Oid(r.provariadic),
                regproc(r.prosupport),
                str_char(r.prokind),
                Value::Bool(r.prosecdef),
                Value::Bool(r.proleakproof),
                Value::Bool(r.proisstrict),
                Value::Bool(r.proretset),
                str_char(r.provolatile),
                str_char(r.proparallel),
                Value::Int2(r.pronargs),
                Value::Int2(0),
                Value::Oid(r.prorettype),
                oidvector(r.proargtypes.iter().copied()),
                optional_array(
                    PgType::Oid,
                    r.proallargtypes.iter().map(|t| Value::Oid(*t)).collect(),
                ),
                optional_array(
                    PgType::Text,
                    r.proargmodes
                        .iter()
                        .map(|m| Value::Text((*m).to_string()))
                        .collect(),
                ),
                optional_array(
                    PgType::Text,
                    r.proargnames
                        .iter()
                        .map(|n| Value::Text((*n).to_string()))
                        .collect(),
                ),
                Value::Null,
                Value::Null,
                Value::Text(r.prosrc.to_string()),
                text_or_null(r.probin),
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        })
        .chain(own)
        .chain(snowball)
        .collect()
}

/// The `pg_proc` rows for the routines this server holds, appended after
/// [`pg_proc_builtin_rows`].
///
/// Honest for everything the catalog actually knows. `probin` is NULL because
/// nothing here is a C function, and `prosupport` is 0 because a user routine
/// has no planner support function — settled, not gaps. `procost`, `prorows`
/// and `proparallel` carry PostgreSQL's own default rather than a zero stub,
/// so they read as they would for a `CREATE FUNCTION` that named no such
/// clause.
///
/// TODO: carry `COST`/`ROWS`/`PARALLEL` into [`CatalogRoutine`] so
/// `procost`/`prorows`/`proparallel` report what the routine declared —
/// `CREATE FUNCTION` parses no `COST`/`ROWS` clause at all and drops the
/// `PARALLEL` it does parse, so one created `PARALLEL SAFE` reports `u`.
/// TODO: fill `pronargdefaults` once argument defaults are accepted;
/// `CREATE FUNCTION` rejects them, so 0 is exact until then.
pub(crate) fn pg_proc_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let mut rows = pg_proc_builtin_rows();
    rows.extend(user_rows(cat.routines(), cat.namespace_oids()));
    rows
}

fn user_rows(
    routines: &[CatalogRoutine],
    namespace_oids: &HashMap<String, u32>,
) -> Vec<Vec<Value>> {
    routines
        .iter()
        .map(|r| {
            vec![
                Value::Oid(r.oid),
                Value::Text(r.name.clone()),
                Value::Oid(
                    namespace_oids
                        .get(&r.namespace)
                        .copied()
                        .unwrap_or(PUBLIC_NAMESPACE_OID),
                ),
                Value::Oid(BOOTSTRAP_ROLE_OID),
                Value::Oid(r.lang),
                // PostgreSQL's defaults: 1 for a built-in, 100 for anything
                // whose body it has to run.
                Value::Float4(if r.lang == 12 || r.lang == 13 {
                    1.0
                } else {
                    100.0
                }),
                Value::Float4(if r.retset { 1000.0 } else { 0.0 }),
                // provariadic: the element type of a VARIADIC parameter, 0 for
                // a routine that declares none.
                Value::Oid(r.variadic_elem),
                // prosupport: a user routine has no planner support function.
                Value::Reg(Reg::unresolved(RegKind::Proc, 0)),
                chr(r.kind),
                Value::Bool(r.secdef),
                Value::Bool(false),
                Value::Bool(r.strict),
                Value::Bool(r.retset),
                chr(r.volatile),
                chr('u'),
                Value::Int2(r.arg_types.len() as i16),
                Value::Int2(0),
                Value::Oid(r.ret_type),
                oidvector(r.arg_types.iter().copied()),
                optional_array(
                    PgType::Oid,
                    r.all_arg_types.iter().map(|t| Value::Oid(*t)).collect(),
                ),
                optional_array(
                    PgType::Text,
                    r.arg_modes
                        .iter()
                        .map(|m| Value::Text(m.to_string()))
                        .collect(),
                ),
                optional_array(
                    PgType::Text,
                    r.arg_names.iter().map(|n| Value::Text(n.clone())).collect(),
                ),
                Value::Null,
                Value::Null,
                Value::Text(r.src.clone()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        })
        .collect()
}
