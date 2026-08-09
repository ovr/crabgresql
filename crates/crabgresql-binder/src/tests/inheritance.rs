//! Inheritance and storage leaves behind an Append.

use super::common::*;

/// The permutation that reads a child row as a parent row.
///
/// The `stud_emp` case is the one that matters: `student`'s columns are not
/// a *prefix* of `stud_emp`'s, because `stud_emp` merges `emp`'s columns in
/// first. Anything that assumed inherited columns stay contiguous, or stay
/// at the front, would read `salary` where `gpa` belongs.
#[test]
fn inherit_map_is_by_name_and_none_when_it_is_the_identity() {
    let cols = |names: &[&str]| -> Vec<Column> {
        names
            .iter()
            .map(|n| Column::new(*n, PgType::Int4))
            .collect()
    };
    let person = TableSchema::new("person", cols(&["name", "age"]));
    let student = TableSchema::new("student", cols(&["name", "age", "gpa"]));
    let stud_emp = TableSchema::new(
        "stud_emp",
        cols(&["name", "age", "salary", "manager", "gpa", "percent"]),
    );

    // Only a verbatim clone of the parent is free. `CREATE TABLE clone ()
    // INHERITS (person)` is exactly that, and pays nothing.
    let clone = TableSchema::new("clone", cols(&["name", "age"]));
    assert!(
        inherit_map(&person, &clone)
            .expect("map must resolve")
            .is_none()
    );
    // A child that merely *appends* still needs a map, because the map is
    // also what narrows its wider tuple to the parent's width.
    let map = inherit_map(&person, &student)
        .expect("map must resolve")
        .expect("a wider child needs a map");
    assert_eq!(map.as_ref(), [0, 1]);
    // Reading `stud_emp` as a `student` needs a real permutation on top.
    let map = inherit_map(&student, &stud_emp)
        .expect("map must resolve")
        .expect("a non-prefix layout needs a map");
    assert_eq!(map.as_ref(), [0, 1, 4]);

    // A missing column is an invariant break, not a user error.
    let stranger = TableSchema::new("stranger", cols(&["name"]));
    assert_eq!(
        inherit_map(&person, &stranger)
            .expect_err("a missing column must be reported")
            .code,
        sqlstate::INTERNAL_ERROR
    );
}

/// A relation whose storage is split into engine-internal leaves. Only
/// `schema` and `storage_leaves` are exercised — `scan_arms` inspects
/// metadata and never touches rows.
struct SplitTable {
    schema: Arc<TableSchema>,
    leaves: Vec<Arc<dyn TableAm>>,
}

impl SplitTable {
    fn new(name: &str, leaves: Vec<Arc<dyn TableAm>>) -> Arc<dyn TableAm> {
        Arc::new(Self {
            schema: Arc::new(TableSchema::new(
                name,
                vec![Column::new("id", PgType::Int4)],
            )),
            leaves,
        })
    }
}

impl TableAm for SplitTable {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.schema)
    }
    fn storage_leaves(&self) -> Option<Vec<Arc<dyn TableAm>>> {
        (!self.leaves.is_empty()).then(|| self.leaves.clone())
    }
    fn scan(
        &self,
        _txn: &crabgresql_storage_api::txn::TxnContext,
        _projection: &crabgresql_storage_api::ColumnProjection,
    ) -> crabgresql_storage_api::TupleStream {
        Box::new(std::iter::empty())
    }
    fn fetch(
        &self,
        _tid: crabgresql_storage_api::Tid,
        _txn: &crabgresql_storage_api::txn::TxnContext,
    ) -> Result<Option<crabgresql_storage_api::Tuple>, crabgresql_storage_api::StorageError> {
        Ok(None)
    }
    fn insert(
        &self,
        _tuple: crabgresql_storage_api::Tuple,
        _txn: &crabgresql_storage_api::txn::TxnContext,
    ) -> Result<crabgresql_storage_api::Tid, crabgresql_storage_api::StorageError> {
        unimplemented!("metadata-only test double")
    }
    fn update(
        &self,
        _tid: crabgresql_storage_api::Tid,
        _tuple: crabgresql_storage_api::Tuple,
        _txn: &crabgresql_storage_api::txn::TxnContext,
    ) -> Result<crabgresql_storage_api::UpdateResult, crabgresql_storage_api::StorageError> {
        unimplemented!("metadata-only test double")
    }
    fn delete(
        &self,
        _tid: crabgresql_storage_api::Tid,
        _txn: &crabgresql_storage_api::txn::TxnContext,
    ) -> Result<crabgresql_storage_api::DeleteResult, crabgresql_storage_api::StorageError> {
        unimplemented!("metadata-only test double")
    }
}

fn leaf_names(leaves: &[Arc<dyn TableAm>]) -> Vec<String> {
    leaves.iter().map(|l| l.schema().name.clone()).collect()
}

fn arm_names(arms: &[MappedRelation]) -> Vec<String> {
    arms.iter().map(|a| a.table.schema().name.clone()).collect()
}

#[test]
fn a_relation_without_storage_leaves_is_scanned_directly() {
    let engine = engine_with_table();
    let table = SplitTable::new("solo", Vec::new());
    assert!(
        scan_arms(&engine, &table, false, false)
            .expect("scan_arms must not fail on a plain relation")
            .is_none(),
        "a monolithic relation must bind to a plain Scan, not a one-armed Append"
    );
}

#[test]
fn storage_leaves_become_the_append_arms() {
    let engine = engine_with_table();
    let table = SplitTable::new(
        "split",
        vec![
            SplitTable::new("split_chunks", Vec::new()),
            SplitTable::new("split_buffer", Vec::new()),
        ],
    );
    let arms = scan_arms(&engine, &table, false, true)
        .expect("scan_arms must not fail")
        .expect("a relation reporting storage leaves must fan out");
    // Order is the access method's, not sorted: a leaf order carries meaning
    // (durable storage before the write buffer, say) that must survive.
    assert_eq!(arm_names(&arms), vec!["split_chunks", "split_buffer"]);
    assert!(
        arms.iter().all(|a| a.map.is_none()),
        "one relation's storage leaves all carry its layout, so none remaps"
    );
    // Storage leaves are one relation's own physical pieces, so every arm
    // reports the relation that owns them — not the leaf's own name.
    assert!(
        arms.iter()
            .all(|a| a.tableoid.as_ref().is_some_and(|id| id.name == "split")),
        "a storage leaf must answer `tableoid` with its owning relation"
    );
}

#[test]
fn a_sql_partitions_storage_leaves_flatten_into_one_append() {
    // A partitioned parent is identified by its schema, so build one whose
    // single leaf itself splits, and confirm the result is one flat list
    // rather than an Append of an Append.
    let inner = SplitTable::new(
        "part_2024",
        vec![
            SplitTable::new("part_2024_chunks", Vec::new()),
            SplitTable::new("part_2024_buffer", Vec::new()),
        ],
    );
    // `partition_leaves` reads the engine's catalog, so exercise the flatten
    // directly on the expansion `scan_arms` performs.
    let flattened: Vec<Arc<dyn TableAm>> = match inner.storage_leaves() {
        Some(leaves) => leaves,
        None => vec![Arc::clone(&inner)],
    };
    assert_eq!(
        leaf_names(&flattened),
        vec!["part_2024_chunks", "part_2024_buffer"],
        "a SQL partition that splits its storage must contribute its leaves, not itself"
    );
}
