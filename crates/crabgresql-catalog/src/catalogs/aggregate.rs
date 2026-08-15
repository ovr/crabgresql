//! `pg_aggregate`: what makes an aggregate function an aggregate.
//!
//! # The deviation this relation has to state
//!
//! The same one `catalogs::operator` carries. Aggregates are implemented by
//! `crabgresql-binder`'s `AggFn` and the executor's own accumulators; neither
//! reads this table, and neither calls the transition and final functions it
//! names. So a row here says what upstream's `avg(int8)` *is* — an aggregate
//! whose state is `internal`, folded by `int8_avg_accum` and finished by
//! `numeric_poly_avg` — and not that this server evaluates it that way, or at
//! all.
//!
//! The 165 rows are still the right answer: the row is upstream's statement
//! about the aggregate, `\da` reads it, and `aggfnoid` points at a `pg_proc`
//! row this build already publishes. What cannot be measured is the converse —
//! how many of the 165 this server has no implementation for — because the
//! aggregate registry is a Rust enum, not a table. The direction that *can* be
//! checked is: every aggregate this server evaluates has a row here, and every
//! reference out of that row lands. See the crate's
//! `pg_aggregate_describes_upstreams_aggregates`.

use crabgresql_storage_api::TableSchema;
use crabgresql_types::{PgType, Value};

use crate::cols::*;
use crate::{PG_AGGREGATE_ROWS, SystemCatalog};

pub(crate) fn pg_aggregate_schema() -> TableSchema {
    TableSchema::in_namespace(
        "pg_aggregate",
        "pg_catalog",
        vec![
            // No `oid`: the aggregate is keyed by the function it extends.
            col("aggfnoid", REGPROC),
            col("aggkind", CHARLIKE),
            col("aggnumdirectargs", PgType::Int2),
            col("aggtransfn", REGPROC),
            col("aggfinalfn", REGPROC),
            col("aggcombinefn", REGPROC),
            col("aggserialfn", REGPROC),
            col("aggdeserialfn", REGPROC),
            col("aggmtransfn", REGPROC),
            col("aggminvtransfn", REGPROC),
            col("aggmfinalfn", REGPROC),
            col("aggfinalextra", PgType::Bool),
            col("aggmfinalextra", PgType::Bool),
            col("aggfinalmodify", CHARLIKE),
            col("aggmfinalmodify", CHARLIKE),
            col("aggsortop", PgType::Oid),
            col("aggtranstype", PgType::Oid),
            col("aggtransspace", PgType::Int4),
            col("aggmtranstype", PgType::Oid),
            col("aggmtransspace", PgType::Int4),
            col("agginitval", PgType::Text),
            col("aggminitval", PgType::Text),
        ],
    )
}

pub(crate) fn pg_aggregate_rows(_cat: &SystemCatalog) -> Vec<Vec<Value>> {
    PG_AGGREGATE_ROWS
        .iter()
        .map(|r| {
            vec![
                regproc(r.aggfnoid),
                str_char(r.aggkind),
                Value::Int2(r.aggnumdirectargs),
                regproc(r.aggtransfn),
                regproc(r.aggfinalfn),
                regproc(r.aggcombinefn),
                regproc(r.aggserialfn),
                regproc(r.aggdeserialfn),
                regproc(r.aggmtransfn),
                regproc(r.aggminvtransfn),
                regproc(r.aggmfinalfn),
                Value::Bool(r.aggfinalextra),
                Value::Bool(r.aggmfinalextra),
                str_char(r.aggfinalmodify),
                str_char(r.aggmfinalmodify),
                Value::Oid(r.aggsortop),
                Value::Oid(r.aggtranstype),
                Value::Int4(r.aggtransspace),
                Value::Oid(r.aggmtranstype),
                Value::Int4(r.aggmtransspace),
                text_or_null(r.agginitval),
                text_or_null(r.aggminitval),
            ]
        })
        .collect()
}
