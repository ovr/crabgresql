//! Range and prefix probes against the durable heap engine's physical B-tree:
//! the boundary rules on a composite key, prefix equality, NULL bounds,
//! descending key columns, and a differential check of every shape against a
//! sequential scan.
//!
//! The oracle throughout is a scan of the same relation applying the same
//! predicate with `compare_values`. A probe that disagrees with it is a wrong
//! answer to a query, which is the only failure mode this whole layer has.

use crabgresql_storage_api::{
    Column, ColumnProjection, IndexBound, IndexKey, IndexMetadata, IndexMethod, IndexProbeKey,
    TableAm, TableEngine, TableSchema,
};
use crabgresql_txn::{CommandId, TransactionManager, TxnContext, Xid};
use crabgresql_types::{PgType, Value};

mod common;
use common::open;

/// `t(a int4, b int4, c int4)` — the shape that exposes the boundary rules: a
/// composite key whose bounded column is followed by another.
fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            Column::new("a", PgType::Int4),
            Column::new("b", PgType::Int4),
            Column::new("c", PgType::Int4),
        ],
    )
}

/// An index over `(a, b, c)`, each column ascending unless named in `descending`.
fn index_on_abc(descending: &[usize]) -> IndexMetadata {
    IndexMetadata {
        name: "t_abc_idx".into(),
        method: IndexMethod::BTree,
        keys: (0..3)
            .map(|column| IndexKey {
                column,
                descending: descending.contains(&column),
                nulls_first: false,
            })
            .collect(),
        unique: false,
        nulls_distinct: true,
        constraint: None,
    }
}

fn read(tm: &TransactionManager) -> TxnContext {
    tm.context(Xid::INVALID, CommandId::FIRST)
}

fn insert_committed(tm: &TransactionManager, table: &dyn TableAm, row: [i32; 3]) {
    let x = tm.allocate_xid();
    table
        .insert(
            row.map(Value::Int4).to_vec(),
            &tm.context(x, CommandId::FIRST),
        )
        .expect("insert");
    tm.commit(x).expect("commit");
}

/// One end of a range, as a test writes it.
#[derive(Clone, Copy)]
struct Bound {
    value: i32,
    inclusive: bool,
}

const fn incl(value: i32) -> Option<Bound> {
    Some(Bound {
        value,
        inclusive: true,
    })
}

const fn excl(value: i32) -> Option<Bound> {
    Some(Bound {
        value,
        inclusive: false,
    })
}

/// Probe `(eq, lower, upper)` and return the matching rows, sorted.
fn probe(
    table: &dyn TableAm,
    txn: &TxnContext,
    eq: &[i32],
    lower: Option<Bound>,
    upper: Option<Bound>,
) -> Vec<[i32; 3]> {
    let eq: Vec<Value> = eq.iter().copied().map(Value::Int4).collect();
    let (lo, hi) = (
        lower.map(|b| Value::Int4(b.value)),
        upper.map(|b| Value::Int4(b.value)),
    );
    let key = IndexProbeKey {
        eq: &eq,
        lower: lo.as_ref().map(|value| IndexBound {
            value,
            inclusive: lower.expect("bound present").inclusive,
        }),
        upper: hi.as_ref().map(|value| IndexBound {
            value,
            inclusive: upper.expect("bound present").inclusive,
        }),
    };
    let mut rows: Vec<[i32; 3]> = table
        .index_lookup("t_abc_idx", &key, txn)
        .expect("the index serves the probe")
        .map(|row| ints(&row.expect("probe failed").1))
        .collect();
    rows.sort();
    rows
}

/// The same rows a sequential scan applying the same predicate returns, sorted —
/// the oracle every probe is checked against.
fn scanned(
    table: &dyn TableAm,
    txn: &TxnContext,
    eq: &[i32],
    lower: Option<Bound>,
    upper: Option<Bound>,
) -> Vec<[i32; 3]> {
    // The bounded column is the one after the equality prefix, exactly as the
    // probe contract says.
    let bounded = eq.len();
    let mut rows: Vec<[i32; 3]> = table
        .scan(txn, &ColumnProjection::All)
        .map(|row| ints(&row.expect("scan failed").1))
        .filter(|row| {
            eq.iter().enumerate().all(|(i, want)| row[i] == *want)
                && lower.is_none_or(|b| {
                    row[bounded] > b.value || (b.inclusive && row[bounded] == b.value)
                })
                && upper.is_none_or(|b| {
                    row[bounded] < b.value || (b.inclusive && row[bounded] == b.value)
                })
        })
        .collect();
    rows.sort();
    rows
}

fn ints(tuple: &[Value]) -> [i32; 3] {
    let mut out = [0i32; 3];
    for (slot, value) in out.iter_mut().zip(tuple) {
        *slot = match value {
            Value::Int4(x) => *x,
            other => panic!("unexpected value {other:?}"),
        };
    }
    out
}

/// Every combination this suite checks: an equality prefix and a pair of bounds
/// on the column after it.
const CASES: &[(&[i32], Option<Bound>, Option<Bound>)] = &[
    // Prefix equality: an index on (a, b, c) probed by `a` alone.
    (&[1], None, None),
    (&[], None, None),
    (&[1, 5], None, None),
    (&[1, 5, 2], None, None),
    // The boundary rules on the column after the prefix.
    (&[1], excl(5), None),
    (&[1], incl(5), None),
    (&[1], None, excl(5)),
    (&[1], None, incl(5)),
    (&[1], excl(2), excl(8)),
    (&[1], incl(2), incl(8)),
    (&[1], incl(5), incl(5)),
    (&[1], excl(5), excl(5)),
    // Bounds on the *first* key column, with nothing pinned.
    (&[], excl(1), None),
    (&[], incl(1), incl(2)),
    // Bounds that select nothing, and bounds that select everything.
    (&[1], incl(99), None),
    (&[1], None, excl(-99)),
    (&[1], incl(-99), incl(99)),
];

/// A table holding every `(a, b, c)` in `1..=3 × 1..=9 × 1..=3`, indexed by
/// `(a, b, c)`, so each `(a, b)` prefix has several rows under it — the shape
/// that makes the difference between a bytewise and a prefix-wise bound
/// visible.
type Seeded = (std::sync::Arc<dyn TableAm>, TransactionManager);

fn seeded(dir: &std::path::Path, descending: &[usize]) -> anyhow::Result<Seeded> {
    let (engine, tm) = open(dir)?;
    let table = engine.create_table(schema())?;
    for a in 1..=3 {
        for b in 1..=9 {
            for c in 1..=3 {
                insert_committed(&tm, &*table, [a, b, c]);
            }
        }
    }
    // After the rows, so the build path encodes them as well.
    engine.create_index("public", "t", index_on_abc(descending))?;
    Ok((table, tm))
}

/// The rule the whole design turns on: on a key `(a, b, c)`, `a = 1 AND b > 5`
/// must not return the rows with `b = 5`, even though every one of them encodes
/// to bytes greater than the bound (their `c` follows it). Checked here on its
/// own, ahead of the differential sweep, because a bytewise `>` passes many of
/// the other cases and fails only this one.
#[test]
fn an_exclusive_bound_excludes_the_rows_holding_its_value() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (table, tm) = seeded(dir.path(), &[])?;
    let engine_rows = |lower| probe(&*table, &read(&tm), &[1], lower, None);

    let excluded = engine_rows(excl(5));
    assert!(
        excluded.iter().all(|row| row[1] > 5),
        "an exclusive bound returned rows at the bound: {excluded:?}"
    );
    // The positive control: those rows exist, so their absence above is the rule
    // working rather than the probe returning nothing.
    let included = engine_rows(incl(5));
    assert_eq!(
        included.len() - excluded.len(),
        3,
        "the three `b = 5` rows are exactly what inclusive adds"
    );
    Ok(())
}

/// The upper end's mirror: `b <= 5` must *keep* the `b = 5` rows, which sort
/// after the bound's own bytes.
#[test]
fn an_inclusive_upper_bound_keeps_the_rows_holding_its_value() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (table, tm) = seeded(dir.path(), &[])?;
    let rows = probe(&*table, &read(&tm), &[1], None, incl(5));
    assert_eq!(
        rows.iter().filter(|row| row[1] == 5).count(),
        3,
        "an inclusive upper bound dropped the rows at the bound: {rows:?}"
    );
    Ok(())
}

/// Every case, ascending and descending, against the sequential-scan oracle.
///
/// Descending is not a separate rule but the same one over an inverted
/// encoding, so running the identical table through both directions is what
/// shows the inversion did not change *which* rows a bound selects.
#[test]
fn every_probe_agrees_with_a_sequential_scan() -> anyhow::Result<()> {
    for descending in [&[][..], &[1][..], &[0, 1, 2][..]] {
        let dir = tempfile::tempdir()?;
        let (table, tm) = seeded(dir.path(), descending)?;
        let txn = read(&tm);
        for (eq, lower, upper) in CASES {
            assert_eq!(
                probe(&*table, &txn, eq, *lower, *upper),
                scanned(&*table, &txn, eq, *lower, *upper),
                "probe disagreed with a scan for eq={eq:?} \
                 lower={:?} upper={:?} descending={descending:?}",
                lower.map(|b| (b.value, b.inclusive)),
                upper.map(|b| (b.value, b.inclusive)),
            );
        }
    }
    Ok(())
}

/// A NULL bound matches nothing — `col > NULL` is unknown for every row. The
/// probe must answer that, not treat the bound as absent and return everything
/// above the prefix.
#[test]
fn a_null_bound_matches_nothing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (table, tm) = seeded(dir.path(), &[])?;
    let eq = [Value::Int4(1)];
    for (lower, upper) in [(Some(Value::Null), None), (None, Some(Value::Null))] {
        let key = IndexProbeKey {
            eq: &eq,
            lower: lower.as_ref().map(|value| IndexBound {
                value,
                inclusive: false,
            }),
            upper: upper.as_ref().map(|value| IndexBound {
                value,
                inclusive: false,
            }),
        };
        let rows: Vec<_> = table
            .index_lookup("t_abc_idx", &key, &read(&tm))
            .expect("a NULL bound is served, not declined")
            .collect();
        assert!(rows.is_empty(), "a NULL bound returned {} rows", rows.len());
    }
    Ok(())
}

/// A bound whose value cannot be encoded — a type that is not the key column's —
/// is the opposite case: the tree knows nothing about it, so the probe is
/// declined and the caller scans. Answering "no rows" there would drop rows a
/// sequential scan returns.
#[test]
fn an_unencodable_bound_declines_the_probe() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (table, tm) = seeded(dir.path(), &[])?;
    let eq = [Value::Int4(1)];
    let value = Value::Text("not an int4".into());
    let key = IndexProbeKey {
        eq: &eq,
        lower: Some(IndexBound {
            value: &value,
            inclusive: true,
        }),
        upper: None,
    };
    assert!(
        table.index_lookup("t_abc_idx", &key, &read(&tm)).is_none(),
        "a bound of the wrong type must decline, not answer empty"
    );
    Ok(())
}

/// A range that spans many leaf pages, so the scan has to follow right-links
/// rather than reading one page. Without enough rows the traversal is never
/// exercised at all.
#[test]
fn a_range_spanning_many_leaves_agrees_with_a_scan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (engine, tm) = open(dir.path())?;
    let table = engine.create_table(schema())?;
    const ROWS: i32 = 6_000;
    // Shuffled, so the tree splits mid-page rather than only at its right edge.
    let mut a = 1i32;
    for _ in 0..ROWS {
        insert_committed(&tm, &*table, [a, a % 7, 0]);
        a = (a + 1237) % ROWS;
    }
    engine.create_index("public", "t", index_on_abc(&[]))?;
    let txn = read(&tm);
    for (lower, upper) in [
        (excl(1000), excl(2000)),
        (incl(0), incl(ROWS)),
        (None, incl(10)),
        (excl(ROWS - 5), None),
    ] {
        assert_eq!(
            probe(&*table, &txn, &[], lower, upper),
            scanned(&*table, &txn, &[], lower, upper),
            "multi-leaf range disagreed with a scan"
        );
    }
    Ok(())
}
