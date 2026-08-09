//! Query-tail key columns: the ORDER BY and DISTINCT keys every row-producing
//! node carries.

use crabgresql_types::PgType;

/// One ORDER BY key: an index into the projected tuple, the type its values
/// compare as, and its direction. `column` may address a hidden ("resjunk")
/// column appended past the visible output width when ORDER BY references an
/// expression not in the select list. NULLs order last for ASC, first for DESC
/// (PG defaults).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortKey {
    pub column: usize,
    pub ty: PgType,
    /// The collation ordering this key, derived from the ORDER BY expression.
    /// Only meaningful for a string `ty`; every other type ignores it.
    pub collation: u32,
    pub asc: bool,
    pub nulls_first: bool,
}

/// One key column of a `SELECT DISTINCT` / `DISTINCT ON`. Both forms reduce to
/// deduplicating on a set of columns of the projected tuple: plain `DISTINCT`
/// keys on every visible output column, `DISTINCT ON (…)` on the resolved ON
/// expressions (which, like ORDER BY, may live in a hidden column past the
/// visible width). `column` indexes the projected tuple; `ty` drives the
/// hash/equality (via the same `hash_key`/`keys_equal` the executor uses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistinctKey {
    pub column: usize,
    pub ty: PgType,
}
