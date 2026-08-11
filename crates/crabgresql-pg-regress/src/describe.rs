//! psql's `\d <relation>` for the one shape crabgresql can render completely: an
//! ordinary table with no footers.
//!
//! psql builds its describe output from a fixed sequence of catalog queries — a
//! name lookup, the relation's `pg_class` flags, one row per column, and then a
//! footer query per feature (indexes, constraints, foreign keys, policies,
//! extended statistics, publications, inheritance children, partitions), and it
//! sends most of them unconditionally.
//!
//! So this runs the first three queries verbatim and refuses — falling back to
//! the "metacommand not supported" stub — whenever the relation is one a real
//! psql would print a footer for. A refusal is a visible diff; silently dropping
//! a footer would be an invisible wrong answer. See [`Flags::footerless`] for the
//! exact gate.
//!
//! TODO: render the footers instead of refusing; their queries need
//! `pg_get_indexdef`, `pg_partition_ancestors`, `pg_policy` and `pg_publication`,
//! none of which crabgresql has.
//!
//! The queries are the ones psql 18.4 sends, captured with `psql -E` (the
//! versions of them already pinned as SQL in `suites/smoke/sql/psql_describe.sql`
//! are the same text).

use std::io;
use std::time::Duration;

use crate::client::{Client, QueryEvent};
use crate::format;

/// Run `\d <pattern>`, returning the text psql would print. `None` means the
/// runner declines and the caller should print its stub: an unsupported pattern
/// form, no unique match, or a relation needing a footer.
pub async fn describe(
    client: &mut Client,
    pattern: &str,
    statement_timeout: Duration,
) -> io::Result<Option<String>> {
    // psql's `processSQLNamePattern` also handles quoting, `schema.name`, and
    // `?`/`*`/regex wildcards. Only a bare, unquoted, wildcard-free name is
    // accepted here — enough for a `\d <table>` in a script, and everything else
    // is left to the stub rather than half-translated.
    //
    // TODO: accept the rest of that pattern grammar — quoted identifiers,
    // `schema.name`, and the `?`/`*`/regex wildcards.
    if pattern.is_empty()
        || !pattern
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Ok(None);
    }

    let lookup = format!(
        "SELECT c.oid,
  n.nspname,
  c.relname
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relname OPERATOR(pg_catalog.~) '^({pattern})$' COLLATE pg_catalog.default
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 2, 3;"
    );
    let Some(found) = query_rows(client, &lookup, statement_timeout).await? else {
        return Ok(None);
    };
    // Zero matches make psql print `Did not find any relation named "x".` on
    // stderr, and more than one makes it describe each in turn; neither is
    // reproduced.
    let [row] = found.as_slice() else {
        return Ok(None);
    };
    let (oid, nspname, relname) = (cell(row, 0), cell(row, 1), cell(row, 2));

    let flags_sql = format!(
        "SELECT c.relchecks, c.relkind, c.relhasindex, c.relhasrules, c.relhastriggers, \
         c.relrowsecurity, c.relforcerowsecurity, false AS relhasoids, c.relispartition, '', \
         c.reltablespace, CASE WHEN c.reloftype = 0 THEN '' ELSE \
         c.reloftype::pg_catalog.regtype::pg_catalog.text END, c.relpersistence, c.relreplident, \
         am.amname
FROM pg_catalog.pg_class c
 LEFT JOIN pg_catalog.pg_class tc ON (c.reltoastrelid = tc.oid)
LEFT JOIN pg_catalog.pg_am am ON (c.relam = am.oid)
WHERE c.oid = '{oid}';"
    );
    let Some(flags) = query_rows(client, &flags_sql, statement_timeout).await? else {
        return Ok(None);
    };
    let [row] = flags.as_slice() else {
        return Ok(None);
    };
    let flags = Flags {
        relchecks: cell(row, 0).to_string(),
        relkind: cell(row, 1).to_string(),
        relhasindex: cell(row, 2) == "t",
        relhasrules: cell(row, 3) == "t",
        relhastriggers: cell(row, 4) == "t",
        relrowsecurity: cell(row, 5) == "t",
        relispartition: cell(row, 8) == "t",
        reloftype: cell(row, 11).to_string(),
        relpersistence: cell(row, 12).to_string(),
    };
    if !flags.footerless() {
        return Ok(None);
    }
    // Inheritance is the one footer-worthy relationship no `pg_class` flag
    // reports (PostgreSQL's `relhassubclass` is not in crabgresql's `pg_class`),
    // so ask `pg_inherits` directly, from both sides.
    let inherits = format!(
        "SELECT count(*) FROM pg_catalog.pg_inherits \
         WHERE inhrelid = '{oid}' OR inhparent = '{oid}';"
    );
    let Some(rows) = query_rows(client, &inherits, statement_timeout).await? else {
        return Ok(None);
    };
    if rows.first().map(|row| cell(row, 0)) != Some("0") {
        return Ok(None);
    }

    let columns_sql = format!(
        "SELECT a.attname,
  pg_catalog.format_type(a.atttypid, a.atttypmod),
  (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true)
   FROM pg_catalog.pg_attrdef d
   WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef),
  a.attnotnull,
  (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t
   WHERE c.oid = a.attcollation AND t.oid = a.atttypid AND a.attcollation <> t.typcollation) \
   AS attcollation,
  a.attidentity,
  a.attgenerated
FROM pg_catalog.pg_attribute a
WHERE a.attrelid = '{oid}' AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum;"
    );
    let Some(columns) = query_rows(client, &columns_sql, statement_timeout).await? else {
        return Ok(None);
    };
    // TODO: build the Default cell text psql prints for an identity or a
    // generated column. Neither column kind exists in crabgresql, so declining
    // costs nothing today, and printing the cell empty would be a wrong answer.
    if columns
        .iter()
        .any(|row| !cell(row, 5).is_empty() || !cell(row, 6).is_empty())
    {
        return Ok(None);
    }

    let title = format!("{} \"{nspname}.{relname}\"", flags.title_noun());
    let rows: Vec<Vec<Option<String>>> = columns
        .iter()
        .map(|row| {
            vec![
                Some(cell(row, 0).to_string()),
                Some(cell(row, 1).to_string()),
                Some(cell(row, 4).to_string()),
                Some(if cell(row, 3) == "t" { "not null" } else { "" }.to_string()),
                Some(cell(row, 2).to_string()),
            ]
        })
        .collect();
    Ok(Some(format::format_describe(
        &title,
        &["Column", "Type", "Collation", "Nullable", "Default"],
        &rows,
    )))
}

/// The `pg_class` columns psql's second describe query projects, narrowed to the
/// ones that decide whether a footer follows.
struct Flags {
    relchecks: String,
    relkind: String,
    relhasindex: bool,
    relhasrules: bool,
    relhastriggers: bool,
    relrowsecurity: bool,
    relispartition: bool,
    reloftype: String,
    relpersistence: String,
}

impl Flags {
    /// Whether a real psql would print the column table and nothing else.
    ///
    /// Every other relkind has a footer of its own — a view its definition, an
    /// index its access method and expressions, a sequence its type range and
    /// "Owned by", a partitioned table its partition key — and each of the flags
    /// here introduces one: `relhasindex` the "Indexes:" block (which is also how
    /// a PRIMARY KEY or UNIQUE constraint shows up), `relchecks` the "Check
    /// constraints:", `relispartition` the "Partition of:", `reloftype` the "Typed
    /// table of:". Foreign keys, policies, extended statistics, publications and
    /// triggers cannot exist in crabgresql at all, but the flags that would
    /// announce three of them are checked anyway so this stays correct if they
    /// arrive.
    fn footerless(&self) -> bool {
        self.relkind == "r"
            && self.relchecks == "0"
            && !self.relhasindex
            && !self.relhasrules
            && !self.relhastriggers
            && !self.relrowsecurity
            && !self.relispartition
            && self.reloftype.is_empty()
    }

    /// psql names the relation's kind in the title, and marks an UNLOGGED table.
    fn title_noun(&self) -> &'static str {
        if self.relpersistence == "u" {
            "Unlogged table"
        } else {
            "Table"
        }
    }
}

/// Send one internal query and return its rows. `None` if the server reported an
/// error, sent no result set, or the statement timed out — every one of which
/// means this `\d` cannot be completed, so the caller falls back to the stub
/// rather than printing half a description.
async fn query_rows(
    client: &mut Client,
    sql: &str,
    statement_timeout: Duration,
) -> io::Result<Option<Vec<Vec<Option<String>>>>> {
    let events = match tokio::time::timeout(statement_timeout, client.simple_query(sql)).await {
        Ok(events) => events?,
        Err(_) => return Ok(None),
    };
    let mut rows = Vec::new();
    let mut described = false;
    for event in events {
        match event {
            QueryEvent::RowDescription(_) => described = true,
            QueryEvent::Row(row) => rows.push(row),
            QueryEvent::Error(_) => return Ok(None),
            _ => {}
        }
    }
    Ok(described.then_some(rows))
}

/// One cell of a result row, with SQL NULL read as the empty string — which is
/// what psql prints for every describe cell that can be NULL (a column with no
/// default, no explicit collation).
fn cell(row: &[Option<String>], index: usize) -> &str {
    row.get(index).and_then(Option::as_deref).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain, empty, unindexed table — the only thing this module renders.
    fn plain() -> Flags {
        Flags {
            relchecks: "0".into(),
            relkind: "r".into(),
            relhasindex: false,
            relhasrules: false,
            relhastriggers: false,
            relrowsecurity: false,
            relispartition: false,
            reloftype: String::new(),
            relpersistence: "p".into(),
        }
    }

    /// Each of these makes a real psql print a footer this module cannot build,
    /// so it must decline and let the caller stub. A PRIMARY KEY or UNIQUE
    /// constraint arrives here as `relhasindex`.
    #[test]
    fn anything_with_a_footer_is_declined() {
        assert!(plain().footerless());
        let cases: Vec<Flags> = vec![
            Flags {
                relkind: "v".into(),
                ..plain()
            },
            Flags {
                relkind: "p".into(),
                ..plain()
            },
            Flags {
                relkind: "S".into(),
                ..plain()
            },
            Flags {
                relchecks: "1".into(),
                ..plain()
            },
            Flags {
                relhasindex: true,
                ..plain()
            },
            Flags {
                relhasrules: true,
                ..plain()
            },
            Flags {
                relhastriggers: true,
                ..plain()
            },
            Flags {
                relrowsecurity: true,
                ..plain()
            },
            Flags {
                relispartition: true,
                ..plain()
            },
            Flags {
                reloftype: "some_type".into(),
                ..plain()
            },
        ];
        for flags in cases {
            assert!(
                !flags.footerless(),
                "relkind {} was not declined",
                flags.relkind
            );
        }
    }

    /// Persistence does not change whether a footer follows, only the noun.
    #[test]
    fn an_unlogged_table_is_named_as_one() {
        assert_eq!(plain().title_noun(), "Table");
        let unlogged = Flags {
            relpersistence: "u".into(),
            ..plain()
        };
        assert!(unlogged.footerless());
        assert_eq!(unlogged.title_noun(), "Unlogged table");
    }
}
