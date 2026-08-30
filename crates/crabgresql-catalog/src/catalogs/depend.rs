//! `pg_depend`: the dependency graph between database objects.
//!
//! Every other relation in this crate reports a *fact* about one object.
//! `pg_depend` reports the edges between them, and it is the one relation where
//! an empty answer would be worse than no answer at all: `pg_dump` reads the
//! graph to order a restore, so an empty `pg_depend` does not read as "nothing
//! depends on anything", it produces a dump that will not restore.
//!
//! So the rows here are **derived**, not stored. Every edge comes out of the
//! same snapshot that already numbers the objects it connects —
//! [`SystemCatalog::relation_oids`], `index_oids`, `toast_oids`,
//! `constraint_oids`, `rewrite_oids`, `attrdef_oids` — plus the schema facts
//! `pg_inherits` reads (`partition_of`, `inherits`, `partition_scheme`) and the
//! column defaults `pg_attrdef` reads. Nothing is invented and nothing is
//! recorded at DDL time: an edge exists here exactly when the state it
//! describes exists.
//!
//! # Shape, as observed on PostgreSQL 18.4
//!
//! The families below were probed against a live 18.4 rather than reasoned
//! about, because several are not what one would guess:
//!
//! - There are **no `deptype = 'p'` rows**. PostgreSQL no longer stores pin
//!   rows, and dependencies *on* a pinned (built-in) object are not recorded
//!   either: a `text` column produces no row, a column of a user-defined enum
//!   produces one.
//! - A `CHECK` constraint produces **two** rows per column it reads — one `a`
//!   (the constraint belongs to the column) and one `n` (its expression names
//!   it). A `CHECK` reading no column produces a single `a` row on the relation
//!   (`refobjsubid = 0`).
//! - An index backing a constraint depends on the *constraint* (`i`); a plain
//!   index depends on the columns it keys (`a`).
//! - A `serial` produces two edges through different objects: the sequence
//!   depends on the column (`a`, from `OWNED BY`), and the column's default
//!   depends on the sequence (`n`, from the `nextval` call). A hand-written
//!   `DEFAULT nextval('s')` produces only the second — which is why
//!   [`crate::CatalogSequence::owned_by`] and the default text are read as two
//!   separate sources here.
//!
//! # What this build has no edge for
//!
//! Each is a consequence of state that does not exist, not a shortcut:
//!
//! - **Composite type ↔ relation (`i`).** `pg_type` carries no row for a
//!   relation's composite type (`typrelid` is 0 throughout), so there is no
//!   object at one end of that edge.
//! - **System and bootstrap objects.** `pg_class` reflects user relations only,
//!   so the initdb-created objects that own most of a stock `pg_depend` are not
//!   addressable here.
//! - **Extension membership (`deptype = 'e'`).** The `plpgsql` row in
//!   `pg_extension` exists, but its member functions are not among the
//!   `pg_proc` rows this build serves; a stock cluster has four such rows.
//! - **Foreign-key, exclusion, collation and operator dependencies.** The DDL
//!   that creates them is refused, so no object here has one.
//!
//! Two more edges exist but are **coarser than PostgreSQL's**, and a client
//! reading them for "does anything depend on this column" gets a false no:
//!
//! - A view's dependency on **another view** is recorded at relation
//!   granularity (`refobjsubid = 0`) where PostgreSQL records one row per
//!   column read — probed on 18.4, where `pg_user`'s rule carries eight rows
//!   into `pg_shadow`. TODO: record a view's column reads before its body is
//!   expanded; the columns this build can recover are the *base* relations'.
//! - A relation a view reads only from an **expression subquery** is recorded
//!   at relation granularity too, for the reason
//!   [`crate::CatalogViewDependency`] gives.
//!
//! # Not the graph `DROP` consults
//!
//! The server refuses and cascades drops from its own dependency walk
//! (`query.rs::dependency_graph`), which this relation neither feeds nor reads.
//! They agree on the cases both model, but a client must not predict the
//! outcome of a `DROP` from these rows.

use std::collections::HashMap;

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::SystemCatalog;
use crate::cols::*;
use crate::oids::*;

pub(crate) fn pg_depend_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_depend",
        "pg_catalog",
        vec![
            col("classid", PgType::Oid),
            col("objid", PgType::Oid),
            col("objsubid", PgType::Int4),
            col("refclassid", PgType::Oid),
            col("refobjid", PgType::Oid),
            col("refobjsubid", PgType::Int4),
            col("deptype", CHARLIKE),
        ],
    )
}

/// One edge, before it is rendered. Sorting the finished list on this tuple is
/// what gives the relation a stable read order: PostgreSQL's own is physical
/// (insertion) order, which nothing here can reproduce and no client may rely
/// on anyway.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Dep {
    classid: u32,
    objid: u32,
    objsubid: i32,
    refclassid: u32,
    refobjid: u32,
    refobjsubid: i32,
    deptype: char,
}

/// The dependency graph of everything in this snapshot. See the module docs for
/// which families of edge exist and which cannot.
pub(crate) fn pg_depend_rows(cat: &SystemCatalog) -> Vec<Vec<Value>> {
    let mut deps = Vec::new();
    relation_deps(cat, &mut deps);
    index_deps(cat, &mut deps);
    constraint_deps(cat, &mut deps);
    default_deps(cat, &mut deps);
    sequence_deps(cat, &mut deps);
    toast_deps(cat, &mut deps);
    type_and_routine_deps(cat, &mut deps);
    rewrite_deps(cat, &mut deps);
    deps.sort();
    // An object can reach the same one twice — an index keyed `(a, a)`, say —
    // and PostgreSQL stores one row for it either way, because it records a
    // dependency set rather than a list.
    deps.dedup();
    deps.into_iter()
        .map(|d| {
            vec![
                Value::Oid(d.classid),
                Value::Oid(d.objid),
                Value::Int4(d.objsubid),
                Value::Oid(d.refclassid),
                Value::Oid(d.refobjid),
                Value::Int4(d.refobjsubid),
                chr(d.deptype),
            ]
        })
        .collect()
}

/// An edge between two whole objects.
fn dep(classid: u32, objid: u32, refclassid: u32, refobjid: u32, deptype: char) -> Dep {
    Dep {
        classid,
        objid,
        objsubid: 0,
        refclassid,
        refobjid,
        refobjsubid: 0,
        deptype,
    }
}

/// An edge onto one *column* of a relation: `refobjsubid` is the attnum.
fn dep_on_column(classid: u32, objid: u32, table_oid: u32, attnum: i32, deptype: char) -> Dep {
    Dep {
        classid,
        objid,
        objsubid: 0,
        refclassid: PG_CLASS_CLASS_OID,
        refobjid: table_oid,
        refobjsubid: attnum,
        deptype,
    }
}

/// A one-based `attnum` from a zero-based column position, or `None` past what
/// an `int2` attnum can hold — the cap [`crate::cols::attnum_vector`] explains.
fn attnum(position: usize) -> Option<i32> {
    i16::try_from(position.saturating_add(1))
        .ok()
        .map(i32::from)
}

/// Resolved against the same numbering `pg_class` publishes, so an edge and the
/// row a client joins it to name the same relation.
fn relation_oid_by_name(cat: &SystemCatalog, namespace: &str, name: &str) -> Option<u32> {
    cat.relation_oids()
        .iter()
        .find(|(_, schema)| schema.namespace == namespace && schema.name == name)
        .map(|(oid, _)| *oid)
}

/// Relations to their namespace (`n`), their partition/inheritance parents, and
/// a partitioned table to its own key columns.
///
/// Every live relation gets the namespace edge — table, view and sequence
/// alike. Indexes and TOAST relations do not, matching PostgreSQL, and they are
/// not in this list to begin with: they are numbered in their own blocks.
fn relation_deps(cat: &SystemCatalog, out: &mut Vec<Dep>) {
    let namespaces = cat.namespace_oids();
    for (oid, schema) in cat.relation_oids() {
        if let Some(nsp) = namespaces.get(&schema.namespace) {
            out.push(dep(
                PG_CLASS_CLASS_OID,
                *oid,
                PG_NAMESPACE_CLASS_OID,
                *nsp,
                'n',
            ));
        }
        // A partition is dropped with its parent (`a`); an `INHERITS` child is
        // not (`n`). PostgreSQL draws the line in exactly that place, and the
        // server's own `DROP` walk draws it in the same one.
        if let Some(part) = &schema.partition_of
            && let Some(parent) =
                relation_oid_by_name(cat, &part.parent_namespace, &part.parent_name)
        {
            out.push(dep(
                PG_CLASS_CLASS_OID,
                *oid,
                PG_CLASS_CLASS_OID,
                parent,
                'a',
            ));
        }
        for inherit in &schema.inherits {
            if let Some(parent) = relation_oid_by_name(cat, &inherit.namespace, &inherit.name) {
                out.push(dep(
                    PG_CLASS_CLASS_OID,
                    *oid,
                    PG_CLASS_CLASS_OID,
                    parent,
                    'n',
                ));
            }
        }
        // A partitioned table depends on its own key columns, internally: the
        // edge runs from the column to the relation, so dropping the column is
        // what it refuses. Note the direction — `objsubid` carries the attnum
        // here, not `refobjsubid`.
        if let Some(scheme) = &schema.partition_scheme {
            for key in &scheme.key_columns {
                let Some(att) = attnum(*key) else { continue };
                out.push(Dep {
                    classid: PG_CLASS_CLASS_OID,
                    objid: *oid,
                    objsubid: att,
                    refclassid: PG_CLASS_CLASS_OID,
                    refobjid: *oid,
                    refobjsubid: 0,
                    deptype: 'i',
                });
            }
        }
        // A column of a user-defined type depends on that type. Built-in types
        // are pinned and produce no row, which is why this asks the catalog for
        // a *reflected* type rather than testing an OID band.
        for (position, column) in schema.columns.iter().enumerate() {
            let type_oid = column.ty.oid();
            if cat.user_type_ref(type_oid).is_none() {
                continue;
            }
            let Some(att) = attnum(position) else {
                continue;
            };
            out.push(Dep {
                classid: PG_CLASS_CLASS_OID,
                objid: *oid,
                objsubid: att,
                refclassid: PG_TYPE_CLASS_OID,
                refobjid: type_oid,
                refobjsubid: 0,
                deptype: 'n',
            });
        }
    }
}

/// An index depends on the constraint it backs (`i`), or, when it backs none,
/// on each column it keys (`a`).
fn index_deps(cat: &SystemCatalog, out: &mut Vec<Dep>) {
    let constraints = cat.constraint_oids();
    for index in cat.index_oids() {
        if let Some(constraint) = constraints.iter().find(|c| c.index_oid == index.oid) {
            out.push(dep(
                PG_CLASS_CLASS_OID,
                index.oid,
                PG_CONSTRAINT_CLASS_OID,
                constraint.oid,
                'i',
            ));
            continue;
        }
        for key in &index.metadata.keys {
            if let Some(att) = attnum(key.column) {
                out.push(dep_on_column(
                    PG_CLASS_CLASS_OID,
                    index.oid,
                    index.table_oid,
                    att,
                    'a',
                ));
            }
        }
    }
}

/// A constraint depends on the columns it constrains (`a`), and a `CHECK`
/// additionally on each column its expression reads (`n`) — the two rows per
/// column PostgreSQL was observed to store. A constraint over no column depends
/// on the relation as a whole, and a *domain* constraint on the domain.
///
/// The `information_schema` domain checks are chained in for the same reason
/// [`crate::catalogs::constraint::pg_constraint_rows`] chains them: their OIDs
/// are `initdb`'s and sit outside the positional band, so they are never in
/// [`SystemCatalog::constraint_oids`], but the rule for what a domain
/// constraint depends on has to have one home.
fn constraint_deps(cat: &SystemCatalog, out: &mut Vec<Dep>) {
    let bootstrap = crate::info_schema::constraints();
    for constraint in cat.constraint_oids().iter().chain(bootstrap.iter()) {
        // A domain constraint constrains a *type*, so it is auto-dependent on
        // the domain and on no relation — `conrelid` is 0 on one, and reading
        // that 0 as a `pg_class` OID is what used to leave a dangling edge.
        if constraint.type_oid != 0 {
            out.push(dep(
                PG_CONSTRAINT_CLASS_OID,
                constraint.oid,
                PG_TYPE_CLASS_OID,
                constraint.type_oid,
                'a',
            ));
            continue;
        }
        if constraint.columns.is_empty() {
            out.push(dep(
                PG_CONSTRAINT_CLASS_OID,
                constraint.oid,
                PG_CLASS_CLASS_OID,
                constraint.table_oid,
                'a',
            ));
            continue;
        }
        for position in &constraint.columns {
            let Some(att) = attnum(*position) else {
                continue;
            };
            out.push(dep_on_column(
                PG_CONSTRAINT_CLASS_OID,
                constraint.oid,
                constraint.table_oid,
                att,
                'a',
            ));
            // `conkey` of a check *is* the set of columns its expression reads,
            // which is what makes the second row derivable without parsing
            // `conbin`.
            if constraint.contype == "c" {
                out.push(dep_on_column(
                    PG_CONSTRAINT_CLASS_OID,
                    constraint.oid,
                    constraint.table_oid,
                    att,
                    'n',
                ));
            }
        }
    }
}

/// A default depends on its column (`a`), and on any sequence its `nextval`
/// call names (`n`).
fn default_deps(cat: &SystemCatalog, out: &mut Vec<Dep>) {
    for def in cat.attrdef_oids() {
        out.push(dep_on_column(
            PG_ATTRDEF_CLASS_OID,
            def.oid,
            def.table_oid,
            i32::from(def.attnum),
            'a',
        ));
        let Some(namespace) = cat
            .relation_oids()
            .iter()
            .find(|(oid, _)| *oid == def.table_oid)
            .map(|(_, schema)| schema.namespace.as_str())
        else {
            continue;
        };
        if let Some(seq) = nextval_target(&def.expr)
            && let Some(seq_oid) =
                relation_oid_by_name(cat, seq.0.as_deref().unwrap_or(namespace), &seq.1)
        {
            out.push(dep(
                PG_ATTRDEF_CLASS_OID,
                def.oid,
                PG_CLASS_CLASS_OID,
                seq_oid,
                'n',
            ));
        }
    }
}

/// An owned sequence depends on the column that owns it (`a`).
fn sequence_deps(cat: &SystemCatalog, out: &mut Vec<Dep>) {
    for owned in owned_sequences(cat) {
        if let Some(att) = attnum(owned.column) {
            out.push(dep_on_column(
                PG_CLASS_CLASS_OID,
                owned.sequence_oid,
                owned.table_oid,
                att,
                'a',
            ));
        }
    }
}

/// A sequence a column owns.
pub(crate) struct OwnedSequence {
    pub(crate) sequence_oid: u32,
    pub(crate) table_oid: u32,
    /// Zero-based position in the owning relation's column list.
    pub(crate) column: usize,
}

/// Every `OWNED BY` pairing in this snapshot.
///
/// [`crate::CatalogSequence::owned_by`] names the table; the column is the one
/// whose default calls this sequence, which is the only place the pairing is
/// written down — and it is written there for every ownership that can exist,
/// since `serial` is the only thing that creates one. A sequence owned by a
/// table with no such default is not reported rather than guessed at.
///
/// Shared with [`crate::SystemCatalog::serial_sequence`] so
/// `pg_get_serial_sequence` and the `a` edge above are the same claim — the
/// split that keeps `pg_get_indexdef` and `pg_index` in step, applied here.
pub(crate) fn owned_sequences(cat: &SystemCatalog) -> Vec<OwnedSequence> {
    let relations = cat.relation_oids();
    let mut out = Vec::new();
    for (seq_oid, params) in cat.sequence_entries() {
        let Some(owner) = &params.owned_by else {
            continue;
        };
        let Some(seq_schema) = relations
            .iter()
            .find(|(oid, _)| *oid == seq_oid)
            .map(|(_, schema)| schema)
        else {
            continue;
        };
        let Some((table_oid, table)) = relations
            .iter()
            .find(|(_, s)| s.namespace == seq_schema.namespace && s.name == *owner)
        else {
            continue;
        };
        let column = table.columns.iter().position(|column| {
            column
                .default
                .as_deref()
                .and_then(nextval_target)
                .is_some_and(|(namespace, name)| {
                    name == seq_schema.name && namespace.is_none_or(|ns| ns == seq_schema.namespace)
                })
        });
        if let Some(column) = column {
            out.push(OwnedSequence {
                sequence_oid: seq_oid,
                table_oid: *table_oid,
                column,
            });
        }
    }
    out
}

/// A TOAST relation depends on its table, internally: it has no existence apart
/// from it, and PostgreSQL drops the two together with no cascade.
fn toast_deps(cat: &SystemCatalog, out: &mut Vec<Dep>) {
    for toast in cat.toast_oids() {
        out.push(dep(
            PG_CLASS_CLASS_OID,
            toast.oid,
            PG_CLASS_CLASS_OID,
            toast.table_oid,
            'i',
        ));
    }
}

/// User-defined types and routines depend on the schema they were created in.
fn type_and_routine_deps(cat: &SystemCatalog, out: &mut Vec<Dep>) {
    let namespaces = cat.namespace_oids();
    for domain in crate::info_schema::DOMAINS {
        out.push(dep(
            PG_TYPE_CLASS_OID,
            domain.oid,
            PG_NAMESPACE_CLASS_OID,
            crate::info_schema::NAMESPACE_OID,
            'n',
        ));
        // The array type is *internal* to its domain: dropping the domain takes
        // it, and it cannot be dropped on its own.
        out.push(dep(
            PG_TYPE_CLASS_OID,
            domain.array_oid,
            PG_TYPE_CLASS_OID,
            domain.oid,
            'i',
        ));
        // The domain's `CHECK` is auto-dependent on it, but that edge is
        // [`constraint_deps`]'s to draw — one rule, one home, whether `initdb`
        // or `CREATE DOMAIN` made the constraint.
        //
        // No edge to the base type: `int4`, `varchar`, `name` and `timestamptz`
        // are all pinned, and PostgreSQL records no dependency on a pinned
        // object (probed on 18.4).
    }
    for ty in cat.user_types() {
        out.push(dep(
            PG_TYPE_CLASS_OID,
            ty.oid,
            PG_NAMESPACE_CLASS_OID,
            PUBLIC_NAMESPACE_OID,
            'n',
        ));
    }
    for routine in cat.routines() {
        if let Some(nsp) = namespaces.get(&routine.namespace) {
            out.push(dep(
                PG_PROC_CLASS_OID,
                routine.oid,
                PG_NAMESPACE_CLASS_OID,
                *nsp,
                'n',
            ));
        }
    }
}

/// A view's `_RETURN` rule depends on the view internally (`i`) and on what the
/// view's query reads (`n`).
fn rewrite_deps(cat: &SystemCatalog, out: &mut Vec<Dep>) {
    let relations = cat.relation_oids();
    let reads: HashMap<(&str, &str), _> = cat
        .view_dependencies()
        .iter()
        .map(|d| ((d.namespace.as_str(), d.name.as_str()), &d.reads))
        .collect();
    for rule in cat.rewrite_oids() {
        out.push(dep(
            PG_REWRITE_CLASS_OID,
            rule.oid,
            PG_CLASS_CLASS_OID,
            rule.view_oid,
            'i',
        ));
        let Some(view) = relations
            .iter()
            .find(|(oid, _)| *oid == rule.view_oid)
            .map(|(_, schema)| schema)
        else {
            continue;
        };
        let Some(reads) = reads.get(&(view.namespace.as_str(), view.name.as_str())) else {
            continue;
        };
        for read in reads.iter() {
            let Some((base_oid, base)) = relations
                .iter()
                .find(|(_, s)| s.namespace == read.namespace && s.name == read.name)
            else {
                continue;
            };
            let Some(columns) = &read.columns else {
                // The whole relation: what PostgreSQL stores for a query that
                // names a relation without reading a column of it.
                out.push(dep(
                    PG_REWRITE_CLASS_OID,
                    rule.oid,
                    PG_CLASS_CLASS_OID,
                    *base_oid,
                    'n',
                ));
                continue;
            };
            for name in columns {
                let Some(att) = base.column_index(name).and_then(attnum) else {
                    continue;
                };
                out.push(dep_on_column(
                    PG_REWRITE_CLASS_OID,
                    rule.oid,
                    *base_oid,
                    att,
                    'n',
                ));
            }
        }
    }
}

/// The relation a `nextval('…')` default names, as `(namespace, name)` with the
/// namespace left `None` when the reference is unqualified.
///
/// Reads the stored default text rather than a bound tree, which is all a
/// default is here (see [`crabgresql_storage_api::Column::default`]). The
/// literal is unescaped and each part unquoted the way the parser would, so a
/// sequence named `"My Seq"` matches; an unquoted part is lower-cased, because
/// that is what re-parsing the text would do to it.
///
/// Public because `DROP SEQUENCE` asks the same question of the same text when
/// it names the dependent column in its DETAIL: one parser, so the two cannot
/// disagree about which default belongs to which sequence.
///
/// The call is matched as a *token*, not as a substring: the text is scanned
/// left to right with string literals and quoted identifiers skipped whole, so
/// `DEFAULT 'nextval(x)'` names nothing, and `my_nextval('s')` is a different
/// function rather than this one. Only a bare `nextval` or an explicit
/// `pg_catalog.nextval` counts.
pub fn nextval_target(default: &str) -> Option<(Option<String>, String)> {
    let bytes = default.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // A doubled quote inside a literal or a quoted identifier is an
            // escaped quote, not its end.
            b'\'' => i = skip_quoted(bytes, i, b'\''),
            b'"' => i = skip_quoted(bytes, i, b'"'),
            byte if is_ident_byte(byte) => {
                let start = i;
                while i < bytes.len() && is_ident_byte(bytes[i]) {
                    i += 1;
                }
                if default[start..i].eq_ignore_ascii_case("nextval")
                    && qualifier_is_pg_catalog(default, start)
                    && let Some(argument) = literal_argument(&default[i..])
                {
                    return Some(split_qualified(&argument));
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Whether the token starting at `at` is either unqualified or qualified by
/// `pg_catalog` — the two spellings that name PostgreSQL's own function.
fn qualifier_is_pg_catalog(text: &str, at: usize) -> bool {
    let before = text[..at].trim_end();
    let Some(before) = before.strip_suffix('.') else {
        return true;
    };
    let before = before.trim_end();
    let qualifier = before
        .rfind(|c: char| !is_ident_byte(c as u8) && c.is_ascii())
        .map_or(before, |end| &before[end + 1..]);
    qualifier.eq_ignore_ascii_case("pg_catalog") || qualifier == "\"pg_catalog\""
}

/// The single-quoted argument of a call whose name ends where `rest` begins,
/// unescaped. `None` when what follows is not `('…'`.
fn literal_argument(rest: &str) -> Option<String> {
    let rest = rest.trim_start().strip_prefix('(')?.trim_start();
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'\'') {
        return None;
    }
    let mut literal = String::new();
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] != b'\'' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\'' {
                i += 1;
            }
            literal.push_str(&rest[start..i]);
            continue;
        }
        if bytes.get(i + 1) == Some(&b'\'') {
            literal.push('\'');
            i += 2;
            continue;
        }
        return Some(literal);
    }
    None
}

/// Skip a `'…'` literal or a `"…"` identifier starting at `at`, returning the
/// index just past its closing quote.
fn skip_quoted(bytes: &[u8], at: usize, quote: u8) -> usize {
    let mut i = at + 1;
    while i < bytes.len() {
        if bytes[i] != quote {
            i += 1;
        } else if bytes.get(i + 1) == Some(&quote) {
            i += 2;
        } else {
            return i + 1;
        }
    }
    i
}

/// Whether `byte` can appear in an unquoted SQL identifier. Every byte of a
/// multi-byte character counts, which is what keeps the scanner's slices on
/// character boundaries.
fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

/// Split a possibly schema-qualified, possibly double-quoted relation
/// reference into its parts, unquoting each.
fn split_qualified(reference: &str) -> (Option<String>, String) {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = reference.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            '.' if !quoted => parts.push(std::mem::take(&mut current)),
            _ if quoted => current.push(c),
            _ => current.push(c.to_ascii_lowercase()),
        }
    }
    parts.push(current);
    let name = parts.pop().unwrap_or_default();
    (parts.pop(), name)
}
