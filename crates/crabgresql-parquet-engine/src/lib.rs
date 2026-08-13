//! Managed append-only Parquet table access method.
//!
//! A statement batch is written as one or more immutable fragments. Files remain
//! `.pending` until the transaction commits; the engine's finalize hook promotes
//! them to `.parquet` or removes them on abort. MVCC identity lives in file
//! metadata, leaving the physical Parquet schema composed solely of user columns.
//!
//! Fragments are immutable, so the only supported mutation besides INSERT is
//! TRUNCATE, implemented as the directory-level twin of the heap's
//! relfilenode-swap: the truncating transaction stages a fresh, empty
//! `parquet/<new>/` and reads and writes there, while the old directory stays
//! untouched until commit (it is removed on commit, and the staged one on abort).
//! A `.pending`-style rename cannot express "all rows are gone", and a tombstone
//! that merely hides fragments would neither free the space nor reset `relpages`
//! until a vacuum, which is not what TRUNCATE promises.
//!
//! Known divergence, shared with the heap and inherited from
//! [`crabgresql_txn::TableLock`]'s scope: a reader's/writer's shared hold covers one
//! *operation*, not its whole transaction (PostgreSQL holds `RowExclusiveLock` to
//! end-of-transaction). So a TRUNCATE can commit between two statements of another
//! open transaction and discard fragments that transaction had already staged.
//!
//! TODO: scope the table lock to the whole transaction instead of to one operation
//! (in the engine, not as per-AM bookkeeping), which is what closes this hole.

mod buffered;

pub use buffered::BufferedParquetTable;

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use arrow_array::{
    Array, ArrayRef, Date32Array, RecordBatch, RecordBatchOptions, RecordBatchReader,
    TimestampMicrosecondArray,
};
use arrow_schema::{DataType, TimeUnit};
use crabgresql_storage_api::arrow::{arrow_schema, build_batch, decode_row};
use crabgresql_storage_api::sort::{sort_permutation, sortable_layout, take_batch};
use crabgresql_storage_api::{
    BatchStream, ColumnProjection, DeleteResult, IndexMetadata, MAX_PHYSICAL_BLOCK, RelStats,
    RelfilenodeAllocator, StorageError, TableAm, TableCapabilities, TableSchema, Tid, Tuple,
    TupleStream, UpdateResult,
};
use crabgresql_txn::{
    CommandId, Infomask, LockOwner, SharedGuard, TableLock, TupleHeader, TxnContext, Xid,
    satisfies_mvcc,
};
use crabgresql_types::{PgType, Value};
use crabgresql_wal::{RedoContext, RmgrId, RmgrRedo, Wal, WalError};
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::arrow::{ArrowSchemaConverter, ProjectionMask};
use parquet::basic::Compression;
use parquet::file::metadata::{KeyValue, SortingColumn};
use parquet::file::properties::WriterProperties;

pub const RMGR_PARQUET: RmgrId = RmgrId(12);
pub const PARQUET_XID_OBSERVED: u8 = 1;
/// A directory-swap TRUNCATE: see [`encode_truncate`] for the payload.
pub const PARQUET_TRUNCATE: u8 = 2;

const FORMAT_VERSION: &str = "1";
const MAX_FRAGMENT_ROWS: usize = u16::MAX as usize;
const PG_UNIX_EPOCH_DAYS: i32 = 10_957;
const PG_UNIX_EPOCH_MICROS: i64 = 946_684_800_000_000;

const META_VERSION: &str = "crabgresql.format_version";
const META_REL: &str = "crabgresql.relfilenode";
const META_XMIN: &str = "crabgresql.xmin";
const META_CMIN: &str = "crabgresql.cmin";
const META_SCHEMA: &str = "crabgresql.schema";

fn io_error(context: &str, error: impl std::fmt::Display) -> StorageError {
    StorageError::Io(format!("{context}: {error}"))
}

fn corrupt(context: impl Into<String>) -> StorageError {
    StorageError::CorruptData(context.into())
}

fn unsupported(message: impl Into<String>) -> StorageError {
    StorageError::UnsupportedOperation(message.into())
}

fn schema_identity(schema: &TableSchema) -> String {
    schema
        .columns
        .iter()
        .map(|column| format!("{}:{}:{}", column.name, column.ty.oid(), column.typmod))
        .collect::<Vec<_>>()
        .join("|")
}

/// The layout sort key as Parquet row-group metadata — the *only* record a
/// fragment keeps of how its rows are ordered.
///
/// Written only when the sort actually ran, so its presence means "this file is
/// clustered" rather than "this table declares a key". The claim is made in
/// Parquet's own vocabulary rather than in a private footer key because that is
/// the field an outside reader already looks at.
///
/// TODO: read this metadata back, to skip row groups whose key range cannot
/// match and to drive a compaction pass; nothing in this engine consumes it.
///
/// `SortingColumn::column_idx` is a **leaf** index, not the ordinal of a
/// top-level field. [`crabgresql_storage_api::arrow::arrow_type`] maps `timetz`
/// and `interval` to `Struct`s, so either of those ahead of a key column shifts
/// the leaf numbering away from the column position an `IndexKey` carries —
/// which would publish confidently wrong metadata. Ask the converted schema
/// instead, and take a key's first leaf.
///
/// **Float keys order by PostgreSQL's rules, which are Parquet's and not
/// Arrow's.** The sort canonicalizes `-0.0` to `0.0` and every NaN payload to
/// one NaN before comparing, so a fragment can hold `+0.0` before `-0.0`, or
/// two NaN bit patterns in input order. That is non-decreasing under the IEEE
/// comparison Parquet defines for `FLOAT`/`DOUBLE` (which calls the zeros equal
/// and leaves NaN undefined), so the declaration is honest — but a reader that
/// merges or binary-searches under Arrow's *total* order must canonicalize the
/// same way before trusting it.
fn sorting_columns(schema: &TableSchema) -> Result<Vec<SortingColumn>, StorageError> {
    let descriptor = ArrowSchemaConverter::new()
        .convert(&arrow_schema(schema))
        .map_err(|error| io_error("describe Parquet sort key", error))?;
    schema
        .sort_key
        .iter()
        .map(|key| {
            (0..descriptor.num_columns())
                .find(|leaf| descriptor.get_column_root_idx(*leaf) == key.column)
                .map(|leaf| SortingColumn {
                    column_idx: leaf as i32,
                    descending: key.descending,
                    nulls_first: key.nulls_first,
                })
                .ok_or_else(|| {
                    corrupt(format!(
                        "sort key names column {} of a {}-column relation",
                        key.column,
                        schema.columns.len()
                    ))
                })
        })
        .collect()
}

/// Whether a fragment can represent this type. The whitelist is the shared
/// columnar one — a Parquet file is just the durable form of a batch, so a type
/// this engine accepts is exactly a type [`crabgresql_storage_api::arrow`] can
/// encode.
pub fn supports_type(ty: PgType) -> bool {
    crabgresql_storage_api::arrow::supports_type(ty)
}

/// Reject a schema this format cannot represent.
///
/// The error names `schema.access_method` rather than a hard-coded "parquet"
/// because the buffer table shares this whitelist: a row a buffer accepts must
/// always be convertible to a fragment, or a flush would fail long after the
/// `INSERT` that should have been rejected. One whitelist keeps that true, and
/// naming the relation's own method keeps the message honest.
pub fn validate_schema(schema: &TableSchema) -> Result<(), StorageError> {
    if let Some(column) = schema
        .columns
        .iter()
        .find(|column| !supports_type(column.ty))
    {
        return Err(StorageError::UnsupportedType(format!(
            "data type {} is not supported by table access method \"{}\"",
            column.ty.name(),
            schema.access_method.as_str(),
        )));
    }
    // `numeric` is the one type whose *modifier* can put it outside the format,
    // and it does so in two ways. Both are caught here rather than at the first
    // INSERT, because unlike a value that does not fit, this is a property of
    // the declaration, and DDL is where a declaration is judged. Getting this
    // wrong is worse than a late error: the relation's RAM buffer would accept
    // rows all day and the *flush* would fail — in the background, with no
    // statement left to report to.
    for column in &schema.columns {
        if column.ty != PgType::Numeric || column.typmod < 0 {
            continue;
        }
        let (precision, scale) = crabgresql_storage_api::arrow::numeric_decimal(column.typmod);
        // The widest Arrow decimal is 256 bits, which stops at 76 digits, while
        // PostgreSQL's `numeric(p, s)` allows a thousand.
        if i32::from(precision) > crabgresql_storage_api::arrow::NUMERIC_MAX_PRECISION {
            return Err(StorageError::UnsupportedType(format!(
                "numeric precision {precision} exceeds the maximum {} supported by table access \
                 method \"{}\"",
                crabgresql_storage_api::arrow::NUMERIC_MAX_PRECISION,
                schema.access_method.as_str(),
            )));
        }
        // Parquet's DECIMAL is defined only for `0 <= scale <= precision`, but
        // PostgreSQL's runs from -1000 to 1000: `numeric(4,-2)` rounds to
        // hundreds and is perfectly ordinary. Arrow carries a negative scale as
        // far as the batch, so this only surfaces when the file is written —
        // which is why the declaration has to be refused up front.
        if !(0..=i32::from(precision)).contains(&i32::from(scale)) {
            return Err(StorageError::UnsupportedType(format!(
                "numeric scale {scale} is outside 0..{precision}, which table access method \
                 \"{}\" requires",
                schema.access_method.as_str(),
            )));
        }
    }
    Ok(())
}

/// Put a batch of rows into the form this format stores, or reject it.
///
/// The type whitelist ([`validate_schema`]) is not enough for `numeric`: the
/// column's decimal fixes a scale and a digit budget, and PostgreSQL accepts
/// values outside both — `NaN` in any `numeric` column, and in a column with no
/// typmod anything finer than
/// [`arrow::NUMERIC_DEFAULT_SCALE`](crabgresql_storage_api::arrow::NUMERIC_DEFAULT_SCALE).
/// Refusing those here keeps the engine's promise that a row an INSERT
/// acknowledged is a row a flush can write; discovering it at flush time would
/// fail a later, unrelated statement — or, in the buffer's case, a background
/// one with no client to report to.
///
/// The rewrite matters as much as the refusal. A relation is two stores, and
/// only the fragment half round trips through a decimal, so a `numeric` that
/// kept its own display scale in the buffer would print differently before and
/// after a flush. Normalizing on the way in makes the two halves agree by
/// construction — the same reason the epoch rebase lives at one boundary.
///
/// Takes the whole batch rather than a row because *which* columns need this is
/// a property of the schema, not of the row: the ordinals are collected once
/// per call, and a relation with no `numeric` column collects an empty vector —
/// which does not allocate — and returns. Per row, the loop would instead
/// re-scan every column of a hundred-column relation to find nothing.
pub fn store_tuples(schema: &TableSchema, tuples: &mut [Tuple]) -> Result<(), StorageError> {
    let numeric: Vec<usize> = schema
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.ty == PgType::Numeric)
        .map(|(index, _)| index)
        .collect();
    if numeric.is_empty() {
        return Ok(());
    }
    for tuple in tuples {
        for &index in &numeric {
            // `get`, not indexing: a tuple whose width does not match the
            // schema is caught with a proper error where the batch is built,
            // and panicking here would beat it to the report.
            let Some(Value::Numeric(value)) = tuple.get(index) else {
                continue;
            };
            let column = &schema.columns[index];
            match crabgresql_storage_api::arrow::numeric_stored(value, column.typmod) {
                Some(stored) => tuple[index] = Value::Numeric(stored),
                None => {
                    let (precision, scale) =
                        crabgresql_storage_api::arrow::numeric_decimal(column.typmod);
                    return Err(StorageError::NumericFieldOverflow {
                        detail: Some(format!(
                            "Column \"{}\" is stored as numeric({precision},{scale}), \
                             which cannot hold {}.",
                            column.name,
                            value.to_display(),
                        )),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Rebase the temporal columns of `batch` between PostgreSQL's epoch and the
/// Unix epoch Parquet's `Date32`/`Timestamp` logical types are defined in.
///
/// This is the **only** place the two epochs meet. Everywhere above it —
/// including every [`RecordBatch`] handed to the executor — a date is PG days
/// and a timestamp is PG microseconds, per the invariant documented on
/// [`crabgresql_storage_api::arrow`]. Keeping the shift at the file boundary is
/// what stops a relation's two storage leaves from disagreeing: the RAM buffer
/// never sees a file, so if the shift lived anywhere else, half a table's rows
/// would come back displaced by `PG_UNIX_EPOCH_DAYS` (about thirty years) with
/// no error to notice.
///
/// `delta` is added to every non-sentinel value; pass it negated to invert.
/// `i32::MIN`/`i32::MAX` and `i64::MIN`/`i64::MAX` are the ±infinity sentinels
/// and are ordinary bit patterns rather than instants, so they pass through
/// untouched — shifting them would both overflow and turn infinity into a date.
fn rebase_epoch(batch: &RecordBatch, days: i32, micros: i64) -> Result<RecordBatch, StorageError> {
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(batch.num_columns());
    let mut changed = false;
    for column in batch.columns() {
        match column.data_type() {
            DataType::Date32 => {
                let values = required_array::<Date32Array>(column.as_ref(), "date")?;
                let shifted: Date32Array = values.try_unary(|value| {
                    if value == i32::MIN || value == i32::MAX {
                        Ok(value)
                    } else {
                        value
                            .checked_add(days)
                            .ok_or_else(|| corrupt("date epoch conversion overflow"))
                    }
                })?;
                columns.push(Arc::new(shifted));
                changed = true;
            }
            DataType::Timestamp(TimeUnit::Microsecond, zone) => {
                let values =
                    required_array::<TimestampMicrosecondArray>(column.as_ref(), "timestamp")?;
                let shifted: TimestampMicrosecondArray = values.try_unary(|value| {
                    if value == i64::MIN || value == i64::MAX {
                        Ok(value)
                    } else {
                        value
                            .checked_add(micros)
                            .ok_or_else(|| corrupt("timestamp epoch conversion overflow"))
                    }
                })?;
                // `try_unary` drops the zone, and it is what distinguishes
                // `timestamptz` from `timestamp` in the file schema.
                columns.push(match zone {
                    Some(zone) => Arc::new(shifted.with_timezone(zone.clone())) as ArrayRef,
                    None => Arc::new(shifted) as ArrayRef,
                });
                changed = true;
            }
            _ => columns.push(Arc::clone(column)),
        }
    }
    if !changed {
        return Ok(batch.clone());
    }
    let options = RecordBatchOptions::new().with_row_count(Some(batch.num_rows()));
    RecordBatch::try_new_with_options(batch.schema(), columns, &options)
        .map_err(|error| io_error("rebase Arrow record batch", error))
}

/// PG epoch -> Unix epoch, on the way into a fragment.
fn to_file_epoch(batch: &RecordBatch) -> Result<RecordBatch, StorageError> {
    rebase_epoch(batch, PG_UNIX_EPOCH_DAYS, PG_UNIX_EPOCH_MICROS)
}

/// Unix epoch -> PG epoch, on the way out of a fragment.
fn from_file_epoch(batch: &RecordBatch) -> Result<RecordBatch, StorageError> {
    rebase_epoch(batch, -PG_UNIX_EPOCH_DAYS, -PG_UNIX_EPOCH_MICROS)
}

fn required_array<'a, T: 'static>(
    array: &'a dyn Array,
    column: &str,
) -> Result<&'a T, StorageError> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        corrupt(format!(
            "Parquet column \"{column}\" has an unexpected type"
        ))
    })
}

#[derive(Clone, Debug)]
struct Fragment {
    path: PathBuf,
    block: u32,
    /// The transaction that wrote the fragment. Reconciliation keys off this even
    /// for a frozen fragment, whose *visibility* is decided by [`Fragment::frozen`].
    xid: Xid,
    cid: CommandId,
    /// Written by `COPY … FREEZE`: the fragment's rows are visible to every
    /// snapshot, so [`header`] reports `xmin = Xid::FROZEN` for it.
    frozen: bool,
    pending: bool,
}

impl Fragment {
    /// The name this fragment takes once its transaction commits: the same file
    /// with the `.pending` suffix stripped. `None` for an already-promoted one.
    fn promoted_path(&self) -> Option<PathBuf> {
        if !self.pending {
            return None;
        }
        let name = self.path.file_name()?.to_str()?.strip_suffix(".pending")?;
        Some(self.path.with_file_name(name))
    }
}

/// Open a fragment by path, tolerating a concurrent commit's promotion rename.
///
/// A reader lists fragments up front but opens them lazily, and a fragment is
/// visible under MVCC as soon as its transaction is marked committed in the
/// clog — which happens *before* the finalize hook renames `.pending` away. A
/// scan can therefore hold a `.pending` path that has since been promoted; the
/// bytes are unchanged, so retry under the committed name rather than failing
/// the query with a spurious ENOENT.
fn open_fragment_file(fragment: &Fragment) -> Result<File, StorageError> {
    match File::open(&fragment.path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let promoted = fragment
                .promoted_path()
                .ok_or_else(|| io_error("open Parquet fragment", error))?;
            File::open(&promoted).map_err(|error| io_error("open Parquet fragment", error))
        }
        Err(error) => Err(io_error("open Parquet fragment", error)),
    }
}

fn parse_fragment(path: PathBuf) -> Result<Option<Fragment>, StorageError> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(corrupt("Parquet fragment has a non-UTF-8 filename"));
    };
    if name.ends_with(".tmp") {
        return Ok(None);
    }
    let (base, pending) = match name.strip_suffix(".parquet.pending") {
        Some(base) => (base, true),
        None => match name.strip_suffix(".parquet") {
            Some(base) => (base, false),
            None => return Ok(None),
        },
    };
    let mut parts = base.split('-');
    // A fragment block is a physical tid address, so the logical-row-id flag must
    // be clear (see `TID_LOGICAL_FLAG`). Rejecting it here, at the one place a
    // block number is read off disk, keeps a hand-edited or future-format name
    // from producing tids that `fetch` would route to the wrong storage.
    let block = parts
        .next()
        .and_then(|value| u32::from_str_radix(value, 16).ok())
        .filter(|block| *block <= MAX_PHYSICAL_BLOCK)
        .ok_or_else(|| corrupt(format!("invalid Parquet fragment filename \"{name}\"")))?;
    let xid = parts
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Xid)
        .ok_or_else(|| corrupt(format!("invalid Parquet fragment filename \"{name}\"")))?;
    let cid = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .map(CommandId)
        .ok_or_else(|| corrupt(format!("invalid Parquet fragment filename \"{name}\"")))?;
    // An optional trailing `-f` marks a `COPY … FREEZE` fragment. The writing
    // transaction stays in the name ahead of it because that is what
    // `reconcile_pending_in` matches on to promote or unlink the file; only the
    // *visibility* is frozen, and that is what this segment carries. Names
    // written before the segment existed simply lack it and stay readable.
    let frozen = match parts.next() {
        None => false,
        Some("f") => true,
        Some(_) => {
            return Err(corrupt(format!(
                "invalid Parquet fragment filename \"{name}\""
            )));
        }
    };
    if parts.next().is_some() {
        return Err(corrupt(format!(
            "invalid Parquet fragment filename \"{name}\""
        )));
    }
    Ok(Some(Fragment {
        path,
        block,
        xid,
        cid,
        frozen,
        pending,
    }))
}

/// List `dir`'s fragments, ordered by block. A missing directory is an error, not
/// an empty table: the relation's storage having vanished must surface, never read
/// back as "no rows". Paths that legitimately race with a directory being reclaimed
/// use [`remove_dir_all_ok`] or create the directory first.
fn fragments(dir: &Path) -> Result<Vec<Fragment>, StorageError> {
    let entries = std::fs::read_dir(dir).map_err(|error| io_error("read Parquet table", error))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("read Parquet table entry", error))?;
        if let Some(fragment) = parse_fragment(entry.path())? {
            out.push(fragment);
        }
    }
    out.sort_by_key(|fragment| fragment.block);
    Ok(out)
}

/// The filename stem a fragment of `block` written by `txn` takes, shared by the
/// writer and by the cleanup path that has to name a half-written `.tmp`.
///
/// The writer's XID stays in the name even for a frozen fragment, because that is
/// what [`ParquetTable::reconcile_pending_in`] matches on to promote or unlink the
/// file; the `-f` segment is what marks its rows frozen for readers. Encoding
/// `Xid::FROZEN` as the name's XID instead would strand the fragment `.pending`
/// forever — visible, since frozen rows ignore that suffix, and never unlinked on
/// abort.
fn fragment_base(block: u32, txn: &TxnContext) -> String {
    format!(
        "{block:08x}-{}-{}{}",
        txn.xid.0,
        txn.cid.0,
        if txn.freeze_inserts { "-f" } else { "" }
    )
}

fn header(fragment: &Fragment) -> TupleHeader {
    TupleHeader {
        // A frozen fragment reports `Xid::FROZEN` rather than its writer, which is
        // what makes `satisfies_mvcc` show its rows to every snapshot without a
        // commit-log lookup. `fragment.xid` still names the writer for
        // reconciliation; only what readers see changes here.
        xmin: if fragment.frozen {
            Xid::FROZEN
        } else {
            fragment.xid
        },
        xmax: Xid::INVALID,
        cmin: fragment.cid,
        cmax: CommandId::FIRST,
        infomask: Infomask::default(),
    }
}

fn metadata_map(metadata: Option<&Vec<KeyValue>>) -> HashMap<&str, &str> {
    metadata
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.value
                .as_deref()
                .map(|value| (item.key.as_str(), value))
        })
        .collect()
}

/// Open a row reader over one fragment, re-checking the footer identity first.
///
/// **Invariant P1.** Every fragment under `parquet/<r>/` carries `META_REL = r`,
/// so `rel` must be the relfilenode that *named the directory the fragment was
/// listed from* — [`ParquetTable::effective_rel`] for a reader inside a
/// transaction with a staged TRUNCATE, not the live one. Directories are only ever
/// created and removed, never renamed, so no footer has to be rewritten; the price
/// is that passing the wrong generation's id reports perfectly good bytes as
/// [`StorageError::CorruptData`].
///
/// The read is restricted to `projection`, and the reader comes back with its
/// **position map**: entry `i` is the schema ordinal that batch column `i`
/// decodes into. For an unprojected read that is the identity; for a projected
/// one it is the selected ordinals. The map is derived from the reader's own
/// schema by field *name* rather than by assuming the mask preserves the
/// requested order.
fn open_reader(
    schema: &TableSchema,
    rel: u32,
    fragment: &Fragment,
    projection: &ColumnProjection,
) -> Result<(ParquetRecordBatchReader, Arc<[usize]>), StorageError> {
    let file = open_fragment_file(fragment)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|error| corrupt(format!("read Parquet footer: {error}")))?;
    let metadata = metadata_map(builder.metadata().file_metadata().key_value_metadata());
    let expected = [
        (META_VERSION, FORMAT_VERSION.to_string()),
        (META_REL, rel.to_string()),
        (META_XMIN, fragment.xid.0.to_string()),
        (META_CMIN, fragment.cid.0.to_string()),
        (META_SCHEMA, schema_identity(schema)),
    ];
    for (key, value) in expected {
        if metadata.get(key).copied() != Some(value.as_str()) {
            return Err(corrupt(format!(
                "Parquet fragment {} has invalid {key} metadata",
                fragment.path.display()
            )));
        }
    }
    if builder.schema().fields().len() != schema.columns.len() {
        return Err(corrupt(format!(
            "Parquet fragment {} does not match the table schema",
            fragment.path.display()
        )));
    }
    // Built *after* the checks above, which read the file schema from the
    // builder — `with_projection` narrows only the reader's output, so both
    // the field-count check and the `META_SCHEMA` identity stay meaningful.
    //
    // `roots`, not `leaves`: `arrow_type` maps `timetz` and `interval` to a
    // `Struct`, so those columns own several *leaf* descriptors but one root.
    // Root indices are the top-level fields, which the writer lays out 1:1 with
    // `schema.columns` — the invariant the field-count check just enforced.
    let mask = match projection {
        ColumnProjection::All => ProjectionMask::all(),
        ColumnProjection::Some(cols) => {
            ProjectionMask::roots(builder.parquet_schema(), cols.iter().copied())
        }
    };
    let reader = builder
        .with_batch_size(8_192)
        .with_projection(mask)
        .build()
        .map_err(|error| corrupt(format!("open Parquet row reader: {error}")))?;

    let positions: Arc<[usize]> = reader
        .schema()
        .fields()
        .iter()
        .map(|field| {
            schema
                .columns
                .iter()
                .position(|column| column.name == *field.name())
                .ok_or_else(|| {
                    corrupt(format!(
                        "Parquet fragment {} has column \"{}\", which the table schema does not",
                        fragment.path.display(),
                        field.name()
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into();

    Ok((reader, positions))
}

/// The columnar twin of [`ParquetScan`]: the same fragments, in the same order,
/// handed up as batches instead of shredded into rows.
///
/// This is the whole point of the batch path — [`ParquetScan`] holds exactly
/// this `RecordBatch` and takes it apart one cell at a time, so a vectorized
/// consumer would otherwise pay to rebuild what the reader already had.
///
/// No [`Tid`] accompanies a batch, which is why this cannot replace the row
/// scan: `fetch` addresses rows by their ordinal *within a scan*, and DML needs
/// that identity.
struct ParquetBatchScan {
    schema: Arc<TableSchema>,
    /// The relation's `scan_schema`, built once. Every batch is stamped with it,
    /// and deriving it per batch would rebuild a `Field` and a metadata map per
    /// column — on a hundred-column relation that dwarfs the widening.
    stamp: Arc<arrow_schema::Schema>,
    rel: u32,
    projection: ColumnProjection,
    fragments: Vec<Fragment>,
    fragment_index: usize,
    reader: Option<ParquetRecordBatchReader>,
    /// Batch column → schema ordinal for the fragment currently open, rebuilt
    /// each time `reader` is replaced.
    positions: Arc<[usize]>,
    /// As in [`ParquetScan`]: held for the whole iterator life so a concurrent
    /// TRUNCATE cannot remove the directory being read.
    _guard: SharedGuard,
}

impl Iterator for ParquetBatchScan {
    type Item = Result<RecordBatch, StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(reader) = &mut self.reader {
                match reader.next() {
                    Some(Ok(batch)) => {
                        // Two conversions, both once per batch rather than once
                        // per row: out of the file's epoch, and back to the
                        // table's full width.
                        return Some(from_file_epoch(&batch).and_then(|batch| {
                            crabgresql_storage_api::arrow::widen(
                                &self.schema,
                                &self.stamp,
                                &self.positions,
                                &batch,
                            )
                        }));
                    }
                    Some(Err(error)) => {
                        self.reader = None;
                        return Some(Err(corrupt(format!("decode Parquet row group: {error}"))));
                    }
                    None => self.reader = None,
                }
            }
            let fragment = self.fragments.get(self.fragment_index)?;
            self.fragment_index += 1;
            match open_reader(&self.schema, self.rel, fragment, &self.projection) {
                Ok((reader, positions)) => {
                    self.reader = Some(reader);
                    self.positions = positions;
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

struct ParquetScan {
    schema: Arc<TableSchema>,
    rel: u32,
    /// The columns to read off disk. Prunes work only: a mask never changes how
    /// many rows a fragment yields or the order they arrive in, which is what
    /// keeps the `Tid` ordinals below stable and `fetch` able to find them.
    projection: ColumnProjection,
    fragments: Vec<Fragment>,
    fragment_index: usize,
    reader: Option<ParquetRecordBatchReader>,
    /// Batch column → schema ordinal for the fragment currently open, rebuilt
    /// each time `reader` is replaced.
    positions: Arc<[usize]>,
    batch: Option<RecordBatch>,
    batch_row: usize,
    file_row: u32,
    current_block: u32,
    /// Keeps the shared hold for the whole iterator life, so a concurrent
    /// TRUNCATE cannot remove the directory this scan is still reading.
    _guard: SharedGuard,
}

impl Iterator for ParquetScan {
    type Item = Result<(Tid, Tuple), StorageError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(batch) = &self.batch
                && self.batch_row < batch.num_rows()
            {
                let row = self.batch_row;
                self.batch_row += 1;
                let offset = self.file_row + 1;
                self.file_row += 1;
                if offset > u16::MAX as u32 {
                    return Some(Err(corrupt("Parquet fragment exceeds the TID row limit")));
                }
                return Some(
                    decode_row(&self.schema, &self.positions, batch, row).map(|tuple| {
                        (
                            Tid {
                                block: self.current_block,
                                offset: offset as u16,
                            },
                            tuple,
                        )
                    }),
                );
            }
            self.batch = None;
            if let Some(reader) = &mut self.reader {
                match reader.next() {
                    // One vectorized rebase per batch, at the file boundary and
                    // nowhere else — see [`rebase_epoch`].
                    Some(Ok(batch)) => match from_file_epoch(&batch) {
                        Ok(batch) => {
                            self.batch = Some(batch);
                            self.batch_row = 0;
                            continue;
                        }
                        Err(error) => {
                            self.reader = None;
                            return Some(Err(error));
                        }
                    },
                    Some(Err(error)) => {
                        self.reader = None;
                        return Some(Err(corrupt(format!("decode Parquet row group: {error}"))));
                    }
                    None => self.reader = None,
                }
            }
            let fragment = self.fragments.get(self.fragment_index)?.clone();
            self.fragment_index += 1;
            self.current_block = fragment.block;
            self.file_row = 0;
            match open_reader(&self.schema, self.rel, &fragment, &self.projection) {
                Ok((reader, positions)) => {
                    self.reader = Some(reader);
                    self.positions = positions;
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

/// An uncommitted directory-swap TRUNCATE staged by one transaction. Because a
/// TRUNCATE holds the table exclusively until it commits, at most one can exist on
/// a table at a time — hence a single `Option`, not a map.
struct PendingTruncate {
    xid: Xid,
    new_rel: u32,
    /// The lock owner holding the table exclusively — needed to release the hold
    /// from the commit/abort path, which only receives the XID.
    owner: LockOwner,
    /// `next_block` as of the *first* TRUNCATE in this transaction, restored on
    /// abort so a later insert into the surviving directory cannot re-issue a
    /// block number an existing fragment already owns (invariant P2).
    saved_next_block: u32,
}

pub struct ParquetTable {
    schema: Arc<TableSchema>,
    /// The data directory. The table's fragments live in `root/parquet/<rel>`,
    /// which TRUNCATE swaps — so the path is derived, never cached.
    root: PathBuf,
    /// The committed relfilenode: the directory every transaction reads, except
    /// the one holding a pending TRUNCATE (which reads `pending.new_rel`).
    live_rel: AtomicU32,
    /// A staged, not-yet-committed TRUNCATE, if any. `pending` and `has_pending`
    /// are the single source of truth for an in-flight swap and are mutated ONLY
    /// together, through this type's methods, so they never drift.
    pending: RwLock<Option<PendingTruncate>>,
    /// Cheap gate letting the read/write hot path skip the `pending` RwLock read
    /// entirely while no TRUNCATE is in flight — kept in sync with `pending`.
    has_pending: AtomicBool,
    /// A `PARQUET_TRUNCATE` record is in the log whose swap the catalog does not
    /// name yet, so replay must still be able to reach it.
    ///
    /// Deliberately NOT tied to `has_pending`, which `commit_truncate` clears
    /// before this type has even finished its own directory work — and well before
    /// the engine's `swap_relfilenode` makes the swap durable. A checkpoint
    /// sampling in that window would publish a redo point above the record, and a
    /// crash would leave the catalog naming a directory `remove_dir_all_ok` has
    /// already deleted.
    truncate_unreconciled: AtomicBool,
    /// Serializes TRUNCATE (exclusive) against readers/writers (shared).
    lock: Arc<TableLock>,
    wal: Arc<Wal>,
    /// The engine's relfilenode counter, used to name the directory a TRUNCATE
    /// stages. Shared with every other relation so an id can never alias one.
    relfilenodes: Arc<dyn RelfilenodeAllocator>,
    indexes: RwLock<Vec<IndexMetadata>>,
    /// The cached ANALYZE result, tagged with the transaction that measured it —
    /// [`Xid::INVALID`] for a result seeded from the catalog at startup. The tag is
    /// what lets an abort tell a measurement of storage it just destroyed (its own)
    /// from one taken before it, which stays valid.
    analyzed: RwLock<Option<(Xid, u32, f64)>>,
    next_block: Mutex<u32>,
    /// Transactions that have staged `.pending` fragments or a TRUNCATE in this
    /// table and not yet been finalized. The engine's commit/abort hook runs over
    /// every open table, so this lets [`ParquetTable::finish_transaction`] answer
    /// "nothing of mine" from memory instead of paying a directory scan and an
    /// fsync on every transaction end. Empty after a restart, which is correct:
    /// [`ParquetTable::recover`] reconciles leftover pending files directly and
    /// the WAL carries the swap.
    staged_xids: Mutex<HashSet<Xid>>,
}

impl ParquetTable {
    pub fn open(
        root: &Path,
        rel: u32,
        schema: TableSchema,
        indexes: Vec<IndexMetadata>,
        wal: Arc<Wal>,
        relfilenodes: Arc<dyn RelfilenodeAllocator>,
    ) -> Result<Self, StorageError> {
        validate_schema(&schema)?;
        let dir = root.join("parquet").join(rel.to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        Ok(Self {
            schema: Arc::new(schema),
            root: root.to_path_buf(),
            live_rel: AtomicU32::new(rel),
            pending: RwLock::new(None),
            has_pending: AtomicBool::new(false),
            truncate_unreconciled: AtomicBool::new(false),
            lock: Arc::new(TableLock::new()),
            wal,
            relfilenodes,
            indexes: RwLock::new(indexes),
            analyzed: RwLock::new(None),
            next_block: Mutex::new(next_block_in(&dir)?),
            staged_xids: Mutex::new(HashSet::new()),
        })
    }

    fn dir_of(&self, rel: u32) -> PathBuf {
        self.root.join("parquet").join(rel.to_string())
    }

    /// The committed relfilenode — the one the catalog names.
    pub fn relfilenode(&self) -> u32 {
        self.live_rel.load(Ordering::Relaxed)
    }

    fn live_dir(&self) -> PathBuf {
        self.dir_of(self.relfilenode())
    }

    /// Whether a TRUNCATE record of this relation still needs replay to reach it.
    /// Read by the checkpoint; see the field's documentation.
    pub fn truncate_unreconciled(&self) -> bool {
        self.truncate_unreconciled.load(Ordering::Acquire)
    }

    /// The swap is now named by the durable catalog, so replay need not reach the
    /// record any more.
    pub fn truncate_reconciled(&self) {
        self.truncate_unreconciled.store(false, Ordering::Release);
    }

    /// The relfilenode `xid` should read and write: the directory staged by its own
    /// TRUNCATE, else the committed one.
    pub fn effective_rel(&self, xid: Xid) -> u32 {
        if self.has_pending.load(Ordering::Acquire)
            && let Some(p) = self
                .pending
                .read()
                .unwrap_or_else(|_| panic!("rwlock poisoned"))
                .as_ref()
            && p.xid == xid
        {
            return p.new_rel;
        }
        self.relfilenode()
    }

    /// The `(new_rel, owner)` of a TRUNCATE staged by `xid`, if any.
    fn staged_truncate(&self, xid: Xid) -> Option<(u32, LockOwner)> {
        self.pending
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .as_ref()
            .filter(|p| p.xid == xid)
            .map(|p| (p.new_rel, p.owner))
    }

    /// Measure the relation's current on-disk size in 8 KB pages, ignoring any
    /// cached ANALYZE result. `statistics()` deliberately prefers the cached
    /// value; ANALYZE itself must re-measure, or `relpages` would freeze at
    /// whatever the first ANALYZE recorded and never track the table's growth.
    ///
    /// Deliberately lock-free: `TableAm::statistics` has no `TxnContext` (so no
    /// lock owner) and is called while planning, where blocking behind a TRUNCATE's
    /// transaction would stall unrelated queries. The cost is a race with a
    /// committing TRUNCATE, which publishes the new relfilenode before removing the
    /// old directory — so a listing that lost the race is simply retried against the
    /// directory that is live by then.
    pub fn measure_relpages(&self) -> Result<u32, StorageError> {
        let rel = self.relfilenode();
        match relpages_in(&self.dir_of(rel)) {
            Ok(relpages) => Ok(relpages),
            Err(error) => {
                let now = self.relfilenode();
                if now == rel {
                    return Err(error);
                }
                relpages_in(&self.dir_of(now))
            }
        }
    }

    /// Size and row count for ANALYZE, taken under one shared hold from ONE listing
    /// of the fragment directory — the scan below reuses the very fragments that
    /// were measured, rather than re-listing. Two listings would let a fragment
    /// promoted (`.pending` renamed away) in between be stat'd at a path that no
    /// longer exists — silently contributing 0 bytes while its rows still count —
    /// and would let an ANALYZE inside an uncommitted TRUNCATE pair one directory's
    /// rows with another's pages.
    pub fn measure(&self, txn: &TxnContext) -> Result<(u32, f64), StorageError> {
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let visible = self.visible_fragments(rel, txn)?;
        let relpages = relpages_of(&visible);
        let mut rows = 0u64;
        // Only the rows are being counted, so read the narrowest column and skip
        // the rest. The count is unchanged — a mask prunes columns, never rows.
        let projection = ColumnProjection::of([], &self.schema);
        for row in self.scan_over(rel, visible, guard, &projection) {
            row?;
            rows += 1;
        }
        Ok((relpages, rows as f64))
    }

    /// Drop the cached ANALYZE result, returning the relation to never-analyzed —
    /// which is what PostgreSQL reports after a TRUNCATE (`relpages = 0`,
    /// `reltuples = -1`), not a measured zero. Called when the fragments the
    /// measurement described stop existing.
    fn forget_analyzed(&self) {
        *self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = None;
    }

    /// Drop the cached ANALYZE result only if `xid` is the transaction that took
    /// it. Used when a rollback unlinks storage that transaction had staged: its
    /// own measurement covered those fragments, while a measurement taken before
    /// the transaction started still describes what survives.
    fn forget_analyzed_by(&self, xid: Xid) {
        let mut analyzed = self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        if analyzed.is_some_and(|(measured, _, _)| measured == xid) {
            *analyzed = None;
        }
    }

    /// Record a result measured outside any transaction — the catalog's persisted
    /// statistics, replayed into the handle at startup.
    pub fn set_analyzed(&self, relpages: u32, reltuples: f64) {
        self.set_analyzed_by(Xid::INVALID, relpages, reltuples);
    }

    /// Record the result of an ANALYZE run by `xid` (see the `analyzed` field).
    pub fn set_analyzed_by(&self, xid: Xid, relpages: u32, reltuples: f64) {
        *self
            .analyzed
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned")) = Some((xid, relpages, reltuples));
    }

    pub fn add_index(&self, index: IndexMetadata) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .push(index);
    }

    pub fn remove_index(&self, name: &str) {
        self.indexes
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .retain(|index| index.name != name);
    }

    /// Promote (on commit) or unlink (on abort) an already-listed set of pending
    /// fragments. Does not scan the directory or fsync it — the caller owns both,
    /// so a batch of transactions costs one listing and one fsync rather than one
    /// of each per transaction.
    fn reconcile(&self, pending: &[Fragment], committed: bool) -> Result<(), StorageError> {
        for fragment in pending {
            if committed {
                let promoted = fragment
                    .promoted_path()
                    .ok_or_else(|| corrupt("pending Parquet filename is invalid"))?;
                std::fs::rename(&fragment.path, &promoted)
                    .map_err(|error| io_error("promote Parquet fragment", error))?;
            } else {
                std::fs::remove_file(&fragment.path)
                    .map_err(|error| io_error("remove aborted Parquet fragment", error))?;
            }
        }
        Ok(())
    }

    /// Promote or unlink the `.pending` fragments `xid` staged in `dir`, scanning
    /// and fsyncing that one directory. A transaction that staged nothing there
    /// costs the listing and no writes.
    fn reconcile_pending_in(
        &self,
        dir: &Path,
        xid: Xid,
        committed: bool,
    ) -> Result<(), StorageError> {
        let pending: Vec<Fragment> = fragments(dir)?
            .into_iter()
            .filter(|fragment| fragment.pending && fragment.xid == xid)
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        self.reconcile(&pending, committed)?;
        sync_dir(dir)
    }

    /// Reconcile everything `xid` staged in this table: its `.pending` fragments
    /// and, if it ran a TRUNCATE, the directory swap.
    ///
    /// Returns the applied swap when a committed TRUNCATE replaced the directory.
    /// The caller must then persist it with `swap_relfilenode` and release the hold
    /// with [`ParquetTable::release_truncate_lock`] — in that order, and on every
    /// path including a failed persist. Handing the hold back rather than dropping
    /// it here is what keeps a stale catalog write from clobbering a newer
    /// TRUNCATE's: a second TRUNCATE cannot even stage until the hold is released.
    ///
    /// The in-memory state transition happens before any error is returned, so an
    /// error here means "the swap took effect in memory but some file work did
    /// not" — the caller logs it, and the WAL record repairs the catalog at the
    /// next recovery. On every path that returns `Ok(None)`/`Err` the hold is
    /// already released, so no caller can leak it.
    pub fn finish_transaction(
        &self,
        xid: Xid,
        committed: bool,
    ) -> Result<Option<ParquetSwap>, StorageError> {
        // The engine's finalize hook calls this for every open Parquet table on
        // every transaction end. Tables the transaction never wrote to answer
        // from memory here, without touching the filesystem at all.
        let staged = self
            .staged_xids
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .remove(&xid);
        if !staged {
            return Ok(None);
        }
        let Some((new_rel, owner)) = self.staged_truncate(xid) else {
            let outcome = self.reconcile_pending_in(&self.live_dir(), xid, committed);
            if !committed {
                // The fragments just unlinked may have been measured: an ANALYZE run
                // inside this transaction sized its own not-yet-committed fragments
                // (the same rows it counted). Those bytes are gone now, so its cached
                // measurement describes a relation that never existed. A measurement
                // taken before this transaction is untouched — it still describes the
                // fragments that survive.
                self.forget_analyzed_by(xid);
            }
            return outcome.map(|()| None);
        };
        if !committed {
            // The TRUNCATE never happened. Its staged directory goes wholesale, so
            // the fragments inside it need no per-file pass — but fragments this
            // transaction staged *before* the TRUNCATE live in the surviving
            // directory and must be unlinked there. Nothing to persist, so the hold
            // is released here.
            self.abort_truncate(xid);
            let cleaned = self.reconcile_pending_in(&self.live_dir(), xid, false);
            let reclaimed = remove_dir_all_ok(&self.dir_of(new_rel));
            // As above: an ANALYZE run by this transaction measured the staged
            // directory, which is now gone. An older measurement described the
            // surviving directory and stays.
            self.forget_analyzed_by(xid);
            self.lock.release_exclusive(owner);
            return cleaned.and(reclaimed).map(|()| None);
        }
        // Swap first, so `live_dir()` already names the new directory and a failure
        // past this point cannot leave the table reading the directory this commit
        // is about to remove.
        let old = self.commit_truncate(xid);
        let promoted = self.reconcile_pending_in(&self.dir_of(new_rel), xid, true);
        // The old directory's rows are gone as of this commit. Failing to remove it
        // only leaks disk: it is no longer named by the catalog, so
        // `gc_orphan_parquet_dirs` reclaims it at the next boot.
        let reclaimed = old.map_or(Ok(()), |old| remove_dir_all_ok(&self.dir_of(old)));
        match promoted.and(reclaimed) {
            Ok(()) => Ok(Some(ParquetSwap { new_rel, owner })),
            Err(error) => {
                // The caller gets no swap to persist, so it cannot release for us.
                self.lock.release_exclusive(owner);
                Err(error)
            }
        }
    }

    /// Release the exclusive hold a committed TRUNCATE kept (keyed by its lock
    /// owner). Call it after persisting the swap [`ParquetTable::finish_transaction`]
    /// returned, whether or not that persist succeeded.
    pub fn release_truncate_lock(&self, owner: LockOwner) {
        self.lock.release_exclusive(owner);
    }

    /// Apply a committed TRUNCATE: the staged directory becomes the live one.
    /// Returns the superseded relfilenode, or `None` if nothing was staged by `xid`.
    ///
    /// Deliberately leaves `next_block` alone (invariant P2): the truncating
    /// transaction may already have filled blocks in the new directory, and
    /// restarting the counter would hand out a block number a fragment there
    /// already owns — duplicate TIDs in a scan and a wrong row from `fetch`.
    fn commit_truncate(&self, xid: Xid) -> Option<u32> {
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let p = pending.take_if(|p| p.xid == xid)?;
        let old = self.live_rel.swap(p.new_rel, Ordering::Relaxed);
        self.has_pending.store(false, Ordering::Release);
        // `truncate_unreconciled` is deliberately NOT cleared here — see its
        // documentation. The caller still has directory work and a catalog write
        // ahead of it, and the record is the swap's only durable trace until that
        // write lands.
        // The measurement described the directory that just went away.
        self.forget_analyzed();
        Some(old)
    }

    /// Discard a staged TRUNCATE on abort: the live directory keeps its rows, and
    /// the block counter returns to where the first TRUNCATE found it.
    fn abort_truncate(&self, xid: Xid) {
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(|_| panic!("rwlock poisoned"));
        let Some(p) = pending.take_if(|p| p.xid == xid) else {
            return;
        };
        // The swap never happened, so no replay is needed to reconcile it.
        self.truncate_reconciled();
        self.has_pending.store(false, Ordering::Release);
        *self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = p.saved_next_block;
    }

    /// Point the table at `new` after recovery applied a committed swap (the
    /// on-disk catalog lagged the WAL). Clears any stale pending state.
    ///
    /// Re-derives `next_block` from the new directory (invariant P4): the handle
    /// was opened against the *old* one, so a carried-over counter can sit below
    /// the highest block the new directory already holds and collide with it.
    pub fn rebind(&self, new: u32) -> Result<(), StorageError> {
        // Both fallible steps run BEFORE `live_rel` is published: a caller that
        // logs the error and carries on (startup must not abort over one relation)
        // would otherwise leave the table pointing at the new directory while the
        // counter still describes the old one — the block collision P4 exists to
        // prevent.
        let dir = self.dir_of(new);
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        let next_block = next_block_in(&dir)?;
        {
            // `pending` and `has_pending` are cleared under one write guard, so no
            // reader can observe the gate set with nothing behind it.
            let mut pending = self
                .pending
                .write()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            *pending = None;
            self.has_pending.store(false, Ordering::Release);
        }
        // Recovery applied the swap and persisted the catalog: what the pin waited
        // for.
        self.truncate_reconciled();
        *self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = next_block;
        self.live_rel.store(new, Ordering::Relaxed);
        // The seeded ANALYZE result came from the catalog, which described the
        // pre-swap directory; as on the commit path, go back to never-analyzed
        // rather than let it describe the relation we just swapped in.
        self.forget_analyzed();
        Ok(())
    }

    pub fn recover(&self, clog: &crabgresql_txn::Clog) -> Result<(), StorageError> {
        let dir = self.live_dir();
        std::fs::create_dir_all(&dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        // Collect the whole directory before touching it: the reconciliation
        // below renames and unlinks entries, and mutating a directory while a
        // `read_dir` stream over it is still open can silently skip entries,
        // stranding another transaction's fragments as `.pending` forever.
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| io_error("recover Parquet table", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io_error("recover Parquet entry", error))?;
        let mut pending: HashMap<Xid, Vec<Fragment>> = HashMap::new();
        let mut dirty = false;
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("tmp") {
                std::fs::remove_file(path)
                    .map_err(|error| io_error("remove temporary Parquet fragment", error))?;
                dirty = true;
                continue;
            }
            if let Some(fragment) = parse_fragment(path)?
                && fragment.pending
            {
                pending.entry(fragment.xid).or_default().push(fragment);
            }
        }
        // One reconcile pass per distinct transaction, not per file — and a
        // single directory fsync covering all of them.
        for (xid, fragments) in &pending {
            self.reconcile(fragments, clog.is_committed(*xid))?;
            dirty = true;
        }
        if dirty {
            sync_dir(&dir)?;
        }
        // A carried-over counter would be wrong if recovery promoted fragments
        // this handle never saw (it is seeded at `open`, before replay).
        *self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned")) = next_block_in(&dir)?;
        Ok(())
    }

    /// Remove the table's storage: the live directory and, if a TRUNCATE is still
    /// staged, the directory it staged (the catalog never named it, so nothing else
    /// would ever reclaim it in this process).
    ///
    /// Deliberately does NOT take the table lock, matching the heap's DROP path: an
    /// exclusive acquire here would wait for an *uncommitted* TRUNCATE's hold, which
    /// is kept to that transaction's end, so a DROP could block for an unbounded
    /// time on a reactor thread with no timeout. The consequence is the same as the
    /// heap's — a concurrent scan can have its storage removed mid-iteration and
    /// report an I/O error — and closing it needs transactional DDL, not a lock here.
    pub fn drop_storage(&self) -> Result<(), StorageError> {
        let staged = self
            .pending
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .as_ref()
            .map(|p| p.new_rel);
        remove_dir_all_ok(&self.live_dir())?;
        match staged {
            Some(staged) => remove_dir_all_ok(&self.dir_of(staged)),
            None => Ok(()),
        }
    }

    /// Build a scan holding a shared lock for the whole iterator life, so a
    /// concurrent TRUNCATE cannot remove the directory it is still reading.
    ///
    /// The relfilenode is resolved **after** the guard is granted, together with
    /// the fragment listing, and then carried in the iterator. Reading it before
    /// the guard would break invariant P1: `acquire_shared` can block for the
    /// lifetime of a concurrent TRUNCATE's transaction, and the swap that
    /// transaction commits while we wait would leave a pre-lock id describing a
    /// directory that no longer exists — reporting the new directory's perfectly
    /// good fragments as corrupt.
    fn scan_in(
        &self,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Result<ParquetScan, StorageError> {
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let fragments = self.visible_fragments(rel, txn)?;
        Ok(self.scan_over(rel, fragments, guard, projection))
    }

    /// The batch-shaped twin of [`ParquetTable::scan_in`], listing the same
    /// fragments under the same shared hold.
    fn batch_scan_in(
        &self,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Result<ParquetBatchScan, StorageError> {
        let guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        Ok(ParquetBatchScan {
            stamp: crabgresql_storage_api::arrow::scan_schema(&self.schema),
            schema: Arc::clone(&self.schema),
            rel,
            projection: projection.clone(),
            fragments: self.visible_fragments(rel, txn)?,
            fragment_index: 0,
            reader: None,
            positions: Arc::from(Vec::new()),
            _guard: guard,
        })
    }

    /// Scan an already-listed fragment set, taking over the caller's shared hold.
    /// Lets a caller that has both measured and listed (see [`ParquetTable::measure`])
    /// read exactly what it measured.
    fn scan_over(
        &self,
        rel: u32,
        fragments: Vec<Fragment>,
        guard: SharedGuard,
        projection: &ColumnProjection,
    ) -> ParquetScan {
        ParquetScan {
            schema: Arc::clone(&self.schema),
            rel,
            projection: projection.clone(),
            fragments,
            fragment_index: 0,
            reader: None,
            positions: Arc::from(Vec::new()),
            batch: None,
            batch_row: 0,
            file_row: 0,
            current_block: 0,
            _guard: guard,
        }
    }

    /// The fragments of `rel`'s directory visible to `txn`. `rel` is passed in
    /// rather than re-derived so the caller's id and the listed directory are
    /// guaranteed to be the same generation (invariant P1).
    fn visible_fragments(&self, rel: u32, txn: &TxnContext) -> Result<Vec<Fragment>, StorageError> {
        Ok(fragments(&self.dir_of(rel))?
            .into_iter()
            .filter(|fragment| {
                satisfies_mvcc(
                    &header(fragment),
                    &txn.snapshot,
                    &txn.clog,
                    txn.xid,
                    txn.cid,
                )
            })
            .collect())
    }

    /// Write one fragment into `dir` and fsync it, returning its `.tmp` path and
    /// the `.pending` name the caller renames it to.
    ///
    /// `rel` must be the relfilenode that names `dir` (invariant P1) — it is
    /// stamped into the footer and re-checked on every later read, so a
    /// post-TRUNCATE insert has to carry the *staged* directory's id.
    ///
    /// `sorting` is `Some` exactly when `batch`'s rows are in the relation's
    /// layout sort key order, and is what puts that on the record, in Parquet's
    /// own row-group metadata — see [`sorting_columns`].
    fn write_fragment(
        &self,
        rel: u32,
        dir: &Path,
        block: u32,
        batch: &RecordBatch,
        sorting: Option<&[SortingColumn]>,
        txn: &TxnContext,
    ) -> Result<(PathBuf, PathBuf), StorageError> {
        let base = fragment_base(block, txn);
        let temp = dir.join(format!("{base}.tmp"));
        let pending = dir.join(format!("{base}.parquet.pending"));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|error| io_error("create Parquet fragment", error))?;
        let writer_file = file
            .try_clone()
            .map_err(|error| io_error("clone Parquet fragment handle", error))?;
        let metadata = vec![
            KeyValue::new(META_VERSION.to_string(), Some(FORMAT_VERSION.to_string())),
            KeyValue::new(META_REL.to_string(), Some(rel.to_string())),
            KeyValue::new(META_XMIN.to_string(), Some(txn.xid.0.to_string())),
            KeyValue::new(META_CMIN.to_string(), Some(txn.cid.0.to_string())),
            KeyValue::new(META_SCHEMA.to_string(), Some(schema_identity(&self.schema))),
        ];
        let properties = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .set_max_row_group_row_count(Some(MAX_FRAGMENT_ROWS))
            .set_key_value_metadata(Some(metadata))
            .set_sorting_columns(sorting.map(<[SortingColumn]>::to_vec))
            .build();
        // Arrives in PG semantics, and is shifted here — once, per fragment —
        // into the epoch the file format is defined in (see [`rebase_epoch`]).
        // The caller may already have sorted it: `rebase_epoch` adds a constant
        // to every non-sentinel and leaves the ±infinity sentinels at the
        // extremes of Arrow's order, so the shift preserves the order and the
        // sort can happen on either side of it.
        let batch = to_file_epoch(batch)?;
        let mut writer = ArrowWriter::try_new(writer_file, batch.schema(), Some(properties))
            .map_err(|error| io_error("create Parquet writer", error))?;
        writer
            .write(&batch)
            .map_err(|error| io_error("write Parquet fragment", error))?;
        writer
            .close()
            .map_err(|error| io_error("close Parquet fragment", error))?;
        file.sync_all()
            .map_err(|error| io_error("fsync Parquet fragment", error))?;
        Ok((temp, pending))
    }
}

fn sync_dir(dir: &Path) -> Result<(), StorageError> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error("fsync Parquet table directory", error))
}

/// The first block number no fragment in `dir` owns.
fn next_block_in(dir: &Path) -> Result<u32, StorageError> {
    Ok(fragments(dir)?
        .into_iter()
        .map(|fragment| fragment.block)
        .max()
        .unwrap_or(0)
        .saturating_add(1))
}

/// `fragments` measured in 8 KB pages, so `relpages` is comparable to a heap
/// relation's.
fn relpages_of(fragments: &[Fragment]) -> u32 {
    let bytes: u64 = fragments
        .iter()
        .filter_map(|fragment| std::fs::metadata(&fragment.path).ok())
        .map(|metadata| metadata.len())
        .sum();
    bytes.div_ceil(8_192).min(u32::MAX as u64) as u32
}

/// `dir`'s committed fragments measured in 8 KB pages. Pending fragments belong to
/// no committed transaction yet and are excluded — a caller measuring what a
/// specific transaction can see wants [`relpages_of`] over its visible set instead.
fn relpages_in(dir: &Path) -> Result<u32, StorageError> {
    let committed: Vec<Fragment> = fragments(dir)?
        .into_iter()
        .filter(|fragment| !fragment.pending)
        .collect();
    Ok(relpages_of(&committed))
}

/// Remove a fragment directory; an already-absent one is success. Every caller is
/// reclaiming storage that may have been reclaimed by a previous attempt, by a
/// crash-time sweep, or never created at all.
fn remove_dir_all_ok(dir: &Path) -> Result<(), StorageError> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("remove Parquet fragment directory", error)),
    }
}

/// A committed TRUNCATE's applied directory swap, handed to the engine so it can
/// persist the new relfilenode and then release the table's exclusive hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParquetSwap {
    pub new_rel: u32,
    pub owner: LockOwner,
}

/// A directory-swap TRUNCATE replayed from the WAL, awaiting the CLOG's verdict on
/// its transaction. Collected by [`ParquetRedo`] and drained by the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredParquetTruncate {
    pub xid: Xid,
    pub namespace: String,
    pub name: String,
    pub old: u32,
    pub new: u32,
}

/// Encode a [`PARQUET_TRUNCATE`] payload: the relation's old (still-live) and new
/// (staged, empty) fragment directories, plus the relation's schema-qualified name
/// so recovery can rebind the catalog once it knows the transaction's fate. Layout
/// `[old:u32][new:u32][ns_len:u32][ns][name_len:u32][name]`, little-endian.
fn encode_truncate(namespace: &str, name: &str, old: u32, new: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&old.to_le_bytes());
    out.extend_from_slice(&new.to_le_bytes());
    for text in [namespace, name] {
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
    out
}

fn decode_truncate(xid: Xid, payload: &[u8]) -> Result<RecoveredParquetTruncate, WalError> {
    let bad = || WalError::Redo("parquet truncate record: truncated payload".to_string());
    let u32_at = |offset: usize| -> Result<u32, WalError> {
        payload
            .get(offset..offset + 4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
            .ok_or_else(bad)
    };
    let old = u32_at(0)?;
    let new = u32_at(4)?;
    let mut at = 8;
    let mut text = || -> Result<String, WalError> {
        let len = u32_at(at)? as usize;
        at += 4;
        let bytes = payload.get(at..at + len).ok_or_else(bad)?;
        at += len;
        String::from_utf8(bytes.to_vec())
            .map_err(|e| WalError::Redo(format!("parquet truncate record: bad name: {e}")))
    };
    let namespace = text()?;
    let name = text()?;
    Ok(RecoveredParquetTruncate {
        xid,
        namespace,
        name,
        old,
        new,
    })
}

impl TableAm for ParquetTable {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.schema)
    }

    fn capabilities(&self) -> TableCapabilities {
        // Append-only per row — fragments are immutable, so there is no UPDATE and
        // no DELETE — but the whole relation can still be replaced wholesale, which
        // is what TRUNCATE does (a fresh fragment directory swapped in on commit).
        TableCapabilities {
            truncate: true,
            ..TableCapabilities::APPEND_ONLY
        }
    }

    fn indexes(&self) -> Vec<IndexMetadata> {
        self.indexes
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
            .clone()
    }

    fn statistics(&self) -> RelStats {
        if let Some((_, relpages, reltuples)) = *self
            .analyzed
            .read()
            .unwrap_or_else(|_| panic!("rwlock poisoned"))
        {
            return RelStats {
                relpages,
                reltuples,
                analyzed: true,
                // No live page count: sizing this relation means walking its
                // fragment directory, which is more than the planner's
                // per-statement budget. The measured figure stands alone, so a
                // plan here does not rescale by growth the way a heap's does.
                curpages: None,
                columns: Arc::from([]),
            };
        }
        let Ok(relpages) = self.measure_relpages() else {
            return RelStats::unknown(&self.schema);
        };
        RelStats::from_pages(relpages, &self.schema)
    }

    fn scan(&self, txn: &TxnContext, projection: &ColumnProjection) -> TupleStream {
        match self.scan_in(txn, projection) {
            Ok(scan) => Box::new(scan),
            Err(error) => Box::new(std::iter::once(Err(error))),
        }
    }

    fn supports_batch_scan(&self) -> bool {
        true
    }

    fn scan_batches(&self, txn: &TxnContext, projection: &ColumnProjection) -> Option<BatchStream> {
        Some(match self.batch_scan_in(txn, projection) {
            Ok(scan) => Box::new(scan),
            Err(error) => Box::new(std::iter::once(Err(error))),
        })
    }

    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>, StorageError> {
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        let rel = self.effective_rel(txn.xid);
        let Some(fragment) = self
            .visible_fragments(rel, txn)?
            .into_iter()
            .find(|fragment| fragment.block == tid.block)
        else {
            return Ok(None);
        };
        // Always unprojected: `fetch` serves EvalPlanQual re-reads and index
        // point lookups, both of which need the whole row.
        let (mut reader, positions) =
            open_reader(&self.schema, rel, &fragment, &ColumnProjection::All)?;
        let mut ordinal = 1u32;
        for batch in &mut reader {
            let batch =
                batch.map_err(|error| corrupt(format!("decode Parquet row group: {error}")))?;
            for row in 0..batch.num_rows() {
                if ordinal == tid.offset as u32 {
                    // Sliced first: this is a point lookup, so rebasing the
                    // whole row group to return one tuple would scale the cost
                    // of a `fetch` with the fragment rather than with the row.
                    let one = from_file_epoch(&batch.slice(row, 1))?;
                    return decode_row(&self.schema, &positions, &one, 0).map(Some);
                }
                ordinal += 1;
            }
        }
        Ok(None)
    }

    fn insert(&self, tuple: Tuple, txn: &TxnContext) -> Result<Tid, StorageError> {
        let mut tids = self.insert_many(vec![tuple], txn)?;
        tids.pop()
            .ok_or_else(|| corrupt("Parquet insert produced no tuple identifier"))
    }

    fn insert_many(&self, tuples: Vec<Tuple>, txn: &TxnContext) -> Result<Vec<Tid>, StorageError> {
        if tuples.is_empty() {
            return Ok(Vec::new());
        }
        // Before anything is staged: a row that cannot be encoded fails the
        // statement that wrote it, never the file it would have gone into.
        let mut tuples = tuples;
        store_tuples(&self.schema, &mut tuples)?;
        // A frozen fragment is visible the instant it is fsynced: `header` reports
        // `Xid::FROZEN` for it and `visible_fragments` never looks at the `.pending`
        // suffix. What keeps that from being a dirty read is that the fragment
        // lands in a staged TRUNCATE directory no other session lists — which is
        // the same precondition the server checked before authorizing the freeze.
        // Asserting it here too, where it is actually relied upon, so a caller that
        // widens the freeze fails loudly instead of publishing uncommitted rows
        // into the live directory.
        if txn.freeze_inserts && self.staged_truncate(txn.xid).is_none() {
            return Err(StorageError::UnsupportedOperation(format!(
                "cannot write frozen rows into \"{}\": \
                 this transaction has not truncated it",
                self.schema.name
            )));
        }
        // Shared hold for the whole write: a concurrent TRUNCATE must not swap the
        // directory out from under fragments that are already being written into it.
        let _guard = self.lock.acquire_shared(txn.lock_owner);
        // Record the writer before any file appears on disk, so the finalize
        // hook is guaranteed to reconcile this transaction's fragments even if
        // the write fails partway. A stale entry only costs one directory scan.
        self.staged_xids
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"))
            .insert(txn.xid);
        // Resolve the target directory ONCE, before taking `next_block`, and use it
        // for every path below: the block counter, the footer's relfilenode and the
        // fsync all have to describe the same directory (invariant P1).
        let rel = self.effective_rel(txn.xid);
        let dir = self.dir_of(rel);
        let rows = tuples.len();
        let batch = build_batch(&self.schema, &tuples)?;
        // Load-bearing, not tidiness: a `Tuple` is a `Vec<Value>` per row, which
        // outweighs the Arrow image on every schema this engine stores, so
        // releasing it here keeps the sorted path's peak at or under the
        // unsorted one's — the same argument the executor's `SortBatch` makes.
        drop(tuples);
        // Sorting is best-effort by design. A key naming a column Arrow cannot
        // order the way PostgreSQL does (`timetz` and `interval` are structs)
        // leaves the rows in insertion order instead
        // of failing: DDL rejects such a key going forward, but a relation
        // created before that check still has to accept writes, and a flush
        // that failed forever would grow the buffer without bound and surface
        // as backpressure on unrelated inserts. Nothing is lost silently — the
        // row-group sort metadata is written only when the sort actually ran.
        //
        // The permutation and the metadata are decided together, in one `if`:
        // a fragment claiming an order it was not written in is the failure
        // this whole change exists to avoid, and two conditions could drift.
        // Note also that the *whole* insert is permuted, not each fragment —
        // only that makes one write's fragments cover disjoint key ranges.
        let (order, sorting) = if !self.schema.sort_key.is_empty() && sortable_layout(&self.schema)
        {
            (
                Some(sort_permutation(&batch, &self.schema.sort_key)?),
                Some(sorting_columns(&self.schema)?),
            )
        } else {
            (None, None)
        };
        let mut next = self
            .next_block
            .lock()
            .unwrap_or_else(|_| panic!("mutex poisoned"));
        let mut staged = Vec::new();
        let mut tids = vec![
            Tid {
                block: 0,
                offset: 0
            };
            rows
        ];
        for start in (0..rows).step_by(MAX_FRAGMENT_ROWS) {
            let len = MAX_FRAGMENT_ROWS.min(rows - start);
            let block = *next;
            // A fragment block is a physical address, so it must stay below the
            // logical-tid flag (see `TID_LOGICAL_FLAG`) — past it, a fragment tid
            // would read as a logical row id and `fetch` would route it wrong.
            *next = next
                .checked_add(1)
                .filter(|next| *next <= MAX_PHYSICAL_BLOCK)
                .ok_or_else(|| io_error("allocate Parquet fragment", "fragment id exhausted"))?;
            // Gathered one fragment at a time rather than taking the whole
            // permutation up front: the sorted copy then never exceeds a
            // fragment, where a whole-batch `take` would hold a second full
            // image of the insert across every compression and fsync below.
            // Same elements, same order — `order` holds global input positions,
            // and `take` is elementwise. The unsorted path slices instead,
            // which is free: an offset and a length over the same buffers.
            //
            // Chained rather than `?`-ed so a failed gather unwinds through the
            // same cleanup as a failed write: either way this transaction's
            // half-written fragments must not survive the error.
            let written = match &order {
                Some(indices) => take_batch(&batch, &indices.slice(start, len)),
                None => Ok(batch.slice(start, len)),
            }
            .and_then(|fragment| {
                self.write_fragment(rel, &dir, block, &fragment, sorting.as_deref(), txn)
            });
            let (temp, pending) = match written {
                Ok(paths) => paths,
                Err(error) => {
                    let base = fragment_base(block, txn);
                    let _ = std::fs::remove_file(dir.join(format!("{base}.tmp")));
                    for (temp, pending) in &staged {
                        let _ = std::fs::remove_file(temp);
                        let _ = std::fs::remove_file(pending);
                    }
                    return Err(error);
                }
            };
            staged.push((temp, pending));
            // A tid is a physical address, so it is assigned in the order rows
            // were written — but the caller indexes the result by *input*
            // position, so the permutation has to be undone here. `order` is a
            // bijection, so every slot is filled exactly once.
            for row in 0..len {
                let input = match &order {
                    Some(indices) => indices.value(start + row) as usize,
                    None => start + row,
                };
                tids[input] = Tid {
                    block,
                    offset: (row + 1) as u16,
                };
            }
        }
        for (temp, pending) in &staged {
            if let Err(error) = std::fs::rename(temp, pending) {
                for (staged_temp, staged_pending) in &staged {
                    let _ = std::fs::remove_file(staged_temp);
                    let _ = std::fs::remove_file(staged_pending);
                }
                let _ = sync_dir(&dir);
                return Err(io_error("publish pending Parquet fragment", error));
            }
        }
        if let Err(error) = sync_dir(&dir) {
            for (temp, pending) in &staged {
                let _ = std::fs::remove_file(temp);
                let _ = std::fs::remove_file(pending);
            }
            let _ = sync_dir(&dir);
            return Err(error);
        }
        let lsn = self
            .wal
            .append(RMGR_PARQUET, PARQUET_XID_OBSERVED, txn.xid, &[])
            .end;
        if let Err(error) = self.wal.flush(lsn) {
            for (temp, pending) in &staged {
                let _ = std::fs::remove_file(temp);
                let _ = std::fs::remove_file(pending);
            }
            let _ = sync_dir(&dir);
            return Err(io_error("flush Parquet XID WAL record", error));
        }
        Ok(tids)
    }

    fn update(
        &self,
        _tid: Tid,
        _tuple: Tuple,
        _txn: &TxnContext,
    ) -> Result<UpdateResult, StorageError> {
        Err(unsupported(
            "table access method \"parquet\" does not support UPDATE",
        ))
    }

    fn delete(&self, _tid: Tid, _txn: &TxnContext) -> Result<DeleteResult, StorageError> {
        Err(unsupported(
            "table access method \"parquet\" does not support DELETE",
        ))
    }

    /// Transactional TRUNCATE via a fragment-directory swap — the Parquet twin of
    /// the heap's relfilenode swap. Stages a fresh, empty `parquet/<new>/` and holds
    /// the table exclusively until the transaction ends; the swap is applied on
    /// commit and discarded on abort by [`ParquetTable::finish_transaction`]. The
    /// old directory stays intact until commit, so a rollback or a
    /// crash-before-commit restores every row.
    fn truncate(&self, txn: &TxnContext) -> Result<(), StorageError> {
        // AccessExclusiveLock: block concurrent readers/writers of this table until
        // we commit, so no one reads the directory we are about to remove or writes
        // fragments the swap would drop. Held until txn end.
        self.lock.acquire_exclusive(txn.lock_owner);
        let old = self.effective_rel(txn.xid);
        // A fresh, never-reused relfilenode for the empty post-truncate directory.
        let new = self.relfilenodes.alloc_relfilenode();
        let new_dir = self.dir_of(new);
        match self.stage_truncate(old, new, &new_dir, txn) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Nothing was staged, so nothing will ever release this hold on our
                // behalf — unless a previous TRUNCATE in the same transaction owns
                // it, in which case its commit/abort still does. The empty directory
                // is not named by the catalog: `gc_orphan_parquet_dirs` reclaims it.
                let _ = remove_dir_all_ok(&new_dir);
                if self.staged_truncate(txn.xid).is_none() {
                    self.lock.release_exclusive(txn.lock_owner);
                }
                Err(error)
            }
        }
    }

    /// A staged directory swap by `xid` means this transaction's fragments land in
    /// a directory an abort removes wholesale — the discardable storage
    /// `COPY … FREEZE` needs.
    fn truncated_by(&self, xid: Xid) -> bool {
        self.staged_truncate(xid).is_some()
    }
}

impl ParquetTable {
    /// Create the staged directory, WAL-log the swap and record it in memory.
    /// Split out of [`TableAm::truncate`] so every failure before the state
    /// transition takes the same cleanup path.
    fn stage_truncate(
        &self,
        old: u32,
        new: u32,
        new_dir: &Path,
        txn: &TxnContext,
    ) -> Result<(), StorageError> {
        std::fs::create_dir_all(new_dir)
            .map_err(|error| io_error("create Parquet table directory", error))?;
        sync_dir(&self.root.join("parquet"))?;
        // WAL-log the swap intent {old, new, relation} and flush it. Recovery
        // applies the swap only for a committed XID, so the record is safe to write
        // now; and because it carries the XID, a transaction that only TRUNCATEs is
        // still observed by recovery's XID allocator without a separate
        // `PARQUET_XID_OBSERVED` record.
        // Held from the append through every piece of state a checkpoint reads, so
        // a redo point cannot be sampled above a record whose relation still looks
        // unpinned. A block expression, so it ends before the `remove_dir_all_ok`
        // below rather than covering that file I/O too.
        let superseded = {
            let _delay = self.wal.delay_checkpoint();
            let lsn = self.wal.append(
                RMGR_PARQUET,
                PARQUET_TRUNCATE,
                txn.xid,
                &encode_truncate(&self.schema.namespace, &self.schema.name, old, new),
            );
            self.wal
                .flush(lsn.end)
                .map_err(|error| io_error("flush Parquet TRUNCATE WAL record", error))?;
            // Only once the record is durable: a failed flush must leave nothing
            // pinned.
            self.truncate_unreconciled.store(true, Ordering::Release);
            self.staged_xids
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"))
                .insert(txn.xid);
            let mut pending = self
                .pending
                .write()
                .unwrap_or_else(|_| panic!("rwlock poisoned"));
            let mut next = self
                .next_block
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned"));
            // A second TRUNCATE in one transaction keeps the FIRST saved counter: abort
            // must restore what the transaction found, not what its own first TRUNCATE
            // left behind (invariant P2).
            let saved = pending
                .as_ref()
                .filter(|p| p.xid == txn.xid)
                .map_or(*next, |p| p.saved_next_block);
            let superseded = pending.replace(PendingTruncate {
                xid: txn.xid,
                new_rel: new,
                owner: txn.lock_owner,
                saved_next_block: saved,
            });
            self.has_pending.store(true, Ordering::Release);
            // The staged directory is empty, so its fragments start from block 1 again.
            *next = 1;
            drop(next);
            drop(pending);
            superseded
        };
        if let Some(superseded) = superseded {
            // Used only by this uncommitted transaction; reclaim it now.
            let _ = remove_dir_all_ok(&self.dir_of(superseded.new_rel));
        }
        Ok(())
    }
}

/// Replays Parquet WAL records. A [`PARQUET_XID_OBSERVED`] record exists only so
/// the XID allocator observes the transaction — fragment bytes were fsynced before
/// commit and pending-file promotion is reconciled separately. A
/// [`PARQUET_TRUNCATE`] record additionally materializes the staged directory (so
/// the same transaction's later inserts have somewhere to have landed) and records
/// the swap for the engine to resolve once the CLOG is rebuilt.
pub struct ParquetRedo {
    root: PathBuf,
    recovered: Mutex<Vec<RecoveredParquetTruncate>>,
}

impl ParquetRedo {
    pub fn new(data_dir: &Path) -> ParquetRedo {
        ParquetRedo {
            root: data_dir.to_path_buf(),
            recovered: Mutex::new(Vec::new()),
        }
    }

    /// Drain the swaps seen during replay, in WAL order.
    pub fn take_recovered(&self) -> Vec<RecoveredParquetTruncate> {
        std::mem::take(
            &mut *self
                .recovered
                .lock()
                .unwrap_or_else(|_| panic!("mutex poisoned")),
        )
    }
}

impl RmgrRedo for ParquetRedo {
    fn redo(&self, ctx: &RedoContext) -> Result<(), WalError> {
        match ctx.info {
            PARQUET_XID_OBSERVED if ctx.payload.is_empty() => Ok(()),
            PARQUET_TRUNCATE => {
                let record = decode_truncate(ctx.xid, ctx.payload)?;
                let dir = self.root.join("parquet").join(record.new.to_string());
                std::fs::create_dir_all(&dir).map_err(|error| {
                    WalError::Redo(format!(
                        "create Parquet directory {}: {error}",
                        dir.display()
                    ))
                })?;
                self.recovered
                    .lock()
                    .unwrap_or_else(|_| panic!("mutex poisoned"))
                    .push(record);
                Ok(())
            }
            other => Err(WalError::Redo(format!(
                "unknown parquet WAL record info byte {other:#x}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crabgresql_storage_api::arrow::decode_row;
    use crabgresql_storage_api::{
        Column, ColumnProjection, IndexKey, StorageError, TableAccessMethod, TableAm, TableSchema,
        Tid, Tuple,
    };
    use crabgresql_txn::{Clog, CommandId, CommitSink, TransactionManager, TxnContext, Xid};
    use crabgresql_types::numeric::Numeric;
    use crabgresql_types::{Interval, PgType, TimeTz, Value};
    use crabgresql_wal::{RmgrRegistry, Wal, recover};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::basic::Compression;

    use super::{ParquetTable, RelfilenodeAllocator, corrupt, header, parse_fragment};

    /// The frozen marker is an *optional* trailing segment, so fragment files
    /// written before it existed keep parsing — the on-disk format has to stay
    /// readable across upgrades. And a frozen fragment reports its writer as the
    /// owner (reconciliation matches on that) while reporting `Xid::FROZEN` as the
    /// visibility xmin; getting those two the same way round is what keeps an
    /// aborted frozen load from stranding a permanently visible `.pending` file.
    #[test]
    fn fragment_filenames_carry_owner_and_freeze_separately() -> Result<(), StorageError> {
        let parse = |name: &str| parse_fragment(PathBuf::from("/t").join(name));

        let plain = parse("0000002a-77-3.parquet")?.expect("a fragment");
        assert_eq!(
            (plain.block, plain.xid, plain.cid),
            (42, Xid(77), CommandId(3))
        );
        assert!(!plain.frozen && !plain.pending);
        assert_eq!(header(&plain).xmin, Xid(77));

        let frozen = parse("0000002a-77-3-f.parquet.pending")?.expect("a fragment");
        assert_eq!(frozen.xid, Xid(77), "the writer still owns it");
        assert!(frozen.frozen && frozen.pending);
        assert_eq!(header(&frozen).xmin, Xid::FROZEN);

        // A trailing segment that is not the marker is corruption, not a freeze.
        assert!(parse("0000002a-77-3-q.parquet").is_err());
        assert!(parse("0000002a-77-3-f-f.parquet").is_err());
        Ok(())
    }

    /// A relfilenode counter for tests. It starts far above the ids the tests
    /// assign by hand, so a directory staged by a TRUNCATE can never collide with
    /// one of them.
    struct Counter(std::sync::atomic::AtomicU32);

    impl RelfilenodeAllocator for Counter {
        fn alloc_relfilenode(&self) -> u32 {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        }
    }

    fn open_table(
        dir: &Path,
        rel: u32,
        schema: TableSchema,
        wal: Arc<Wal>,
    ) -> Result<ParquetTable, StorageError> {
        ParquetTable::open(
            dir,
            rel,
            schema,
            Vec::new(),
            wal,
            Arc::new(Counter(std::sync::atomic::AtomicU32::new(1_000))),
        )
    }

    /// End a transaction the way the engine's finalize hook does: reconcile, then
    /// release the hold a committed TRUNCATE handed back. Returns the swapped-in
    /// relfilenode, if any.
    fn finish(
        table: &ParquetTable,
        xid: Xid,
        committed: bool,
    ) -> Result<Option<u32>, StorageError> {
        let swap = table.finish_transaction(xid, committed)?;
        if let Some(swap) = swap {
            table.release_truncate_lock(swap.owner);
        }
        Ok(swap.map(|swap| swap.new_rel))
    }

    fn manager(wal: &Arc<Wal>) -> TransactionManager {
        let sink: Arc<dyn CommitSink> = Arc::clone(wal) as Arc<dyn CommitSink>;
        TransactionManager::new_recovered(sink, Arc::new(Clog::new()), Xid::FIRST_NORMAL)
    }

    fn schema(name: &str, types: &[PgType]) -> TableSchema {
        let mut schema = TableSchema::new(
            name,
            types
                .iter()
                .enumerate()
                .map(|(index, ty)| Column::new(format!("c{index}"), *ty))
                .collect(),
        );
        schema.access_method = TableAccessMethod::Parquet;
        schema
    }

    /// Each `numeric` column is normalized at its **own** typmod, and a value
    /// that does not fit names the column it was in.
    ///
    /// The point of the test is the indexing: [`store_tuples`] walks a list of
    /// column ordinals rather than zipping the row against the schema, so a
    /// column and its typmod could drift apart without anything else noticing —
    /// the two `numeric`s below sit at ordinals 1 and 3 with a non-`numeric`
    /// between them, which is the arrangement that catches it.
    #[test]
    fn store_tuples_normalizes_each_numeric_at_its_own_typmod() -> anyhow::Result<()> {
        let mut schema = schema(
            "mixed",
            &[PgType::Int4, PgType::Numeric, PgType::Text, PgType::Numeric],
        );
        schema.columns[1].typmod = Numeric::pack_typmod(10, 2);
        // Column 3 keeps typmod -1: the unconstrained default.

        let mut rows = vec![vec![
            Value::Int4(7),
            Value::Numeric(Numeric::parse("1.5")?.apply_typmod(10, 2)?),
            Value::Text("x".into()),
            Value::Numeric(Numeric::parse("1.5")?),
        ]];
        super::store_tuples(&schema, &mut rows)?;

        let rendered = |value: &Value| match value {
            Value::Numeric(n) => n.to_display(),
            other => panic!("not a numeric: {other:?}"),
        };
        assert_eq!(rendered(&rows[0][1]), "1.50", "column 1 is numeric(10,2)");
        assert_eq!(
            rendered(&rows[0][3]),
            "1.5000000000000000",
            "column 3 has no typmod and is stored at the default scale"
        );
        // The columns either side are untouched.
        assert_eq!(rows[0][0], Value::Int4(7));
        assert_eq!(rows[0][2], Value::Text("x".into()));

        // A refusal names the offending column, not merely the position.
        let mut bad = vec![vec![
            Value::Int4(7),
            Value::Numeric(Numeric::parse("1.5")?.apply_typmod(10, 2)?),
            Value::Text("x".into()),
            Value::Numeric(Numeric::nan()),
        ]];
        let error = super::store_tuples(&schema, &mut bad).expect_err("NaN has no decimal image");
        let detail = match &error {
            StorageError::NumericFieldOverflow { detail } => detail.clone().unwrap_or_default(),
            other => panic!("unexpected error: {other:?}"),
        };
        assert!(detail.contains("\"c3\""), "wrong column named: {detail}");
        Ok(())
    }

    /// A relation with no `numeric` column is not walked row by row at all.
    #[test]
    fn store_tuples_is_a_no_op_without_a_numeric_column() -> anyhow::Result<()> {
        let schema = schema("plain", &[PgType::Int4, PgType::Text]);
        let original = vec![vec![Value::Int4(1), Value::Text("a".into())]];
        let mut rows = original.clone();
        super::store_tuples(&schema, &mut rows)?;
        assert_eq!(rows, original);
        Ok(())
    }

    fn parquet_files(dir: &Path, rel: u32) -> anyhow::Result<Vec<PathBuf>> {
        let table_dir = dir.join("parquet").join(rel.to_string());
        let mut files = Vec::new();
        for entry in std::fs::read_dir(table_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("parquet") {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }

    #[test]
    fn supported_values_round_trip_and_file_has_only_user_columns() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema(
            "types",
            &[
                PgType::Bool,
                PgType::Int2,
                PgType::Int4,
                PgType::Int8,
                PgType::Float4,
                PgType::Float8,
                PgType::Numeric,
                PgType::Text,
                PgType::Varchar,
                PgType::Bpchar,
                PgType::Char,
                PgType::Name,
                PgType::Bytea,
                PgType::Uuid,
                PgType::Date,
                PgType::Time,
                PgType::TimeTz,
                PgType::Timestamp,
                PgType::TimestampTz,
                PgType::Interval,
            ],
        );
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;
        let row = vec![
            Value::Bool(true),
            Value::Int2(-2),
            Value::Int4(42),
            Value::Int8(9_000_000_000),
            Value::Float4(1.25),
            Value::Float8(-2.5),
            Value::Numeric(Numeric::parse("1234567890.012300")?),
            Value::Text("hello".to_string()),
            Value::Text("varchar".to_string()),
            Value::Text("bpchar".to_string()),
            // High-bit: proves the raw byte survives a real Parquet file rather
            // than being smuggled through a UTF-8 column.
            Value::Char(0xFF),
            Value::Text("name".to_string()),
            Value::Bytea(vec![0, 1, 255]),
            Value::Uuid([0x42; 16]),
            Value::Date(9_000),
            Value::Time(12_345_678),
            Value::TimeTz(TimeTz {
                usec: 45_000_000,
                zone: 3_600,
            }),
            Value::Timestamp(123_456_789),
            Value::TimestampTz(-987_654_321),
            Value::Interval(Interval {
                months: 14,
                days: -3,
                usec: 777,
            }),
        ];
        let expected_column_count = row.len();
        let nulls = vec![Value::Null; row.len()];
        let xid = tm.allocate_xid();
        table.insert_many(
            vec![row.clone(), nulls.clone()],
            &tm.context(xid, CommandId::FIRST),
        )?;

        assert_eq!(
            table
                .scan(&tm.context(xid, CommandId::FIRST), &ColumnProjection::All)
                .count(),
            0,
            "a statement cannot see its own inserts before the command counter advances"
        );
        let own_rows: Vec<Tuple> = table
            .scan(&tm.context(xid, CommandId(1)), &ColumnProjection::All)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(own_rows, vec![row.clone(), nulls.clone()]);
        assert_eq!(
            table
                .scan(
                    &tm.context(Xid::INVALID, CommandId::FIRST),
                    &ColumnProjection::All
                )
                .count(),
            0
        );

        tm.commit(xid)?;
        finish(&table, xid, true)?;
        let rows: Vec<Tuple> = table
            .scan(
                &tm.context(Xid::INVALID, CommandId::FIRST),
                &ColumnProjection::All,
            )
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(rows, vec![row, nulls]);

        let files = parquet_files(dir.path(), 1)?;
        assert_eq!(files.len(), 1);
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(&files[0])?)?;
        assert_eq!(builder.schema().fields().len(), expected_column_count);
        assert!(
            builder
                .schema()
                .fields()
                .iter()
                .enumerate()
                .all(|(index, field)| field.name() == &format!("c{index}"))
        );
        assert_eq!(
            builder.metadata().row_group(0).column(0).compression(),
            Compression::SNAPPY
        );
        let metadata = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .ok_or_else(|| anyhow::anyhow!("missing Parquet footer metadata"))?;
        assert!(
            metadata
                .iter()
                .any(|item| item.key == super::META_XMIN && item.value.as_deref() == Some("3"))
        );
        Ok(())
    }

    #[test]
    fn inserts_split_at_fragment_limit_and_tids_fetch_stably() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("many", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        let tuples = (0..=u16::MAX as i32)
            .map(|value| vec![Value::Int4(value)])
            .collect();
        let tids = table.insert_many(tuples, &tm.context(xid, CommandId::FIRST))?;
        assert_eq!(tids.len(), u16::MAX as usize + 1);
        assert_eq!(tids[0], Tid::new(1, 1));
        assert_eq!(tids[u16::MAX as usize - 1], Tid::new(1, u16::MAX));
        assert_eq!(tids[u16::MAX as usize], Tid::new(2, 1));
        tm.commit(xid)?;
        finish(&table, xid, true)?;
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 2);
        assert_eq!(
            table.fetch(Tid::new(2, 1), &tm.context(Xid::INVALID, CommandId::FIRST))?,
            Some(vec![Value::Int4(u16::MAX as i32)])
        );
        Ok(())
    }

    #[test]
    fn aborted_pending_fragments_are_removed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("aborted", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.abort(xid);
        finish(&table, xid, false)?;
        assert!(parquet_files(dir.path(), 1)?.is_empty());
        assert_eq!(
            table
                .scan(
                    &tm.context(Xid::INVALID, CommandId::FIRST),
                    &ColumnProjection::All
                )
                .count(),
            0
        );
        Ok(())
    }

    #[test]
    fn recovery_reconciles_pending_fragments_and_observes_xids() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let committed = open_table(
            dir.path(),
            1,
            schema("committed", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let committed_xid = tm.allocate_xid();
        committed.insert(
            vec![Value::Int4(1)],
            &tm.context(committed_xid, CommandId::FIRST),
        )?;
        tm.commit(committed_xid)?;

        let interrupted = open_table(
            dir.path(),
            2,
            schema("interrupted", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let interrupted_xid = tm.allocate_xid();
        interrupted.insert(
            vec![Value::Int4(2)],
            &tm.context(interrupted_xid, CommandId::FIRST),
        )?;
        drop(committed);
        drop(interrupted);
        drop(tm);
        drop(wal);

        let recovered_wal = Arc::new(Wal::open(dir.path())?);
        let mut registry = RmgrRegistry::new();
        registry.register(
            super::RMGR_PARQUET,
            Arc::new(super::ParquetRedo::new(dir.path())),
        );
        let clog = Arc::new(Clog::new());
        let result = recover(dir.path(), &registry, &clog, crabgresql_wal::Lsn::INVALID)?;
        assert!(result.next_xid > interrupted_xid);

        let committed = open_table(
            dir.path(),
            1,
            schema("committed", &[PgType::Int4]),
            Arc::clone(&recovered_wal),
        )?;
        committed.recover(&clog)?;
        let interrupted = open_table(
            dir.path(),
            2,
            schema("interrupted", &[PgType::Int4]),
            recovered_wal,
        )?;
        interrupted.recover(&clog)?;
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 1);
        assert!(parquet_files(dir.path(), 2)?.is_empty());
        Ok(())
    }

    /// A scan lists fragments up front but opens them lazily, and a fragment
    /// becomes MVCC-visible the moment its transaction is marked committed —
    /// before the finalize hook renames `.pending` away. The reader must follow
    /// the promotion rather than failing the query with a spurious ENOENT.
    #[test]
    fn scan_follows_a_fragment_promoted_after_it_was_listed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("promoted", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(7)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;

        // Snapshot the fragment list (still `.pending`) before the rename lands,
        // exactly as a concurrent session's scan would.
        let scan = table.scan(
            &tm.context(Xid::INVALID, CommandId::FIRST),
            &ColumnProjection::All,
        );
        finish(&table, xid, true)?;
        let rows = scan.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, vec![Value::Int4(7)]);
        Ok(())
    }

    /// `recover` must reconcile *every* pending transaction it finds, including
    /// when several interleave in the same directory. Reconciling from a live
    /// `read_dir` stream while renaming/unlinking entries could skip some.
    #[test]
    fn recover_reconciles_every_pending_transaction() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("interleaved", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        // Interleave several fragments from two transactions, leaving all of
        // them `.pending` as an interrupted run would.
        let (first, second) = (tm.allocate_xid(), tm.allocate_xid());
        for value in 0..8 {
            let xid = if value % 2 == 0 { first } else { second };
            table.insert(vec![Value::Int4(value)], &tm.context(xid, CommandId::FIRST))?;
        }
        assert!(parquet_files(dir.path(), 1)?.is_empty());

        let clog = Clog::new();
        clog.set_committed(first);
        clog.set_aborted(second);
        table.recover(&clog)?;

        // The committed transaction's four fragments were promoted; the aborted
        // transaction's four were unlinked. Neither was left half-reconciled.
        assert_eq!(parquet_files(dir.path(), 1)?.len(), 4);
        let table_dir = dir.path().join("parquet").join("1");
        let pending = std::fs::read_dir(&table_dir)?
            .filter(|entry| {
                entry
                    .as_ref()
                    .is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".pending"))
            })
            .count();
        assert_eq!(pending, 0);
        Ok(())
    }

    /// A frozen fragment is visible as soon as it is fsynced — `header` reports
    /// `Xid::FROZEN` and `visible_fragments` ignores the `.pending` suffix — so the
    /// only thing standing between it and a dirty read is that it lands in a
    /// staged TRUNCATE directory nobody else lists. This asserts the invariant
    /// where it is relied upon, rather than trusting the server two crates away to
    /// have checked it.
    #[test]
    fn a_frozen_write_requires_this_transaction_to_have_truncated() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("frozen", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;

        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST).with_freeze();
        let error = table
            .insert(vec![Value::Int4(1)], &txn)
            .expect_err("a frozen write with no staged truncate must be refused");
        assert!(
            error.to_string().contains("has not truncated it"),
            "{error}"
        );
        // Nothing reached the directory, so no reader could have seen anything.
        assert!(parquet_files(dir.path(), 1)?.is_empty());

        // With the truncate staged by this same transaction it goes through.
        table.truncate(&txn)?;
        table.insert(vec![Value::Int4(1)], &txn)?;
        Ok(())
    }

    /// The finalize hook runs over every open Parquet table on every transaction
    /// end, so a table the transaction never wrote to must answer from memory —
    /// no directory scan, no fsync. Deleting the directory makes any filesystem
    /// access observable as an error.
    #[test]
    fn finish_transaction_skips_tables_the_xid_never_wrote() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("untouched", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        std::fs::remove_dir_all(dir.path().join("parquet").join("1"))?;
        let xid = tm.allocate_xid();
        finish(&table, xid, true)?;
        finish(&table, xid, false)?;
        Ok(())
    }

    /// `statistics()` intentionally returns the last ANALYZE's cached numbers,
    /// so ANALYZE itself must re-measure — otherwise `relpages` freezes at the
    /// first value recorded and never tracks the table's growth.
    #[test]
    fn measure_relpages_ignores_the_cached_analyze_result() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("stats", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let measured = table.measure_relpages()?;
        table.set_analyzed(9_999, 1.0);
        assert_eq!(
            table.statistics().relpages,
            9_999,
            "cache serves statistics"
        );
        assert_eq!(
            table.measure_relpages()?,
            measured,
            "ANALYZE re-measures instead of reading its own cached value back"
        );
        Ok(())
    }

    #[test]
    fn truncated_fragment_is_reported_as_corrupt_storage() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(dir.path(), 1, schema("corrupt", &[PgType::Int4]), wal)?;
        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing committed fragment"))?;
        OpenOptions::new().write(true).open(file)?.set_len(10)?;

        let error = table
            .scan(
                &tm.context(Xid::INVALID, CommandId::FIRST),
                &ColumnProjection::All,
            )
            .next()
            .ok_or_else(|| anyhow::anyhow!("corrupt scan returned no item"))?
            .expect_err("truncated fragment must return an error");
        assert!(matches!(error, StorageError::CorruptData(_)));
        Ok(())
    }

    /// Every `parquet/<n>` directory currently on disk, as relfilenodes.
    fn fragment_dirs(dir: &Path) -> anyhow::Result<Vec<u32>> {
        let mut dirs = Vec::new();
        for entry in std::fs::read_dir(dir.join("parquet"))? {
            if let Some(rel) = entry?
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            {
                dirs.push(rel);
            }
        }
        dirs.sort_unstable();
        Ok(dirs)
    }

    fn scan_values(table: &ParquetTable, txn: &crabgresql_txn::TxnContext) -> Vec<i32> {
        let mut values: Vec<i32> = table
            .scan(txn, &ColumnProjection::All)
            .map(|row| match row.expect("scan row").1.first() {
                Some(Value::Int4(value)) => *value,
                other => panic!("unexpected value {other:?}"),
            })
            .collect();
        values.sort_unstable();
        values
    }

    #[test]
    fn truncate_commit_empties_the_table_and_swaps_the_directory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let truncater = tm.allocate_xid();
        table.truncate(&tm.context(truncater, CommandId::FIRST))?;
        // Read-your-own-truncate: the truncating transaction sees an empty table.
        // (Another session cannot look at all while the TRUNCATE is staged — its
        // AccessShare hold waits for the AccessExclusive one, as in PostgreSQL.)
        assert!(scan_values(&table, &tm.context(truncater, CommandId::FIRST)).is_empty());
        tm.commit(truncater)?;
        let swapped = finish(&table, truncater, true)?.ok_or_else(|| {
            anyhow::anyhow!("a committed TRUNCATE must report its new relfilenode")
        })?;

        assert_eq!(table.relfilenode(), swapped);
        assert_eq!(
            fragment_dirs(dir.path())?,
            vec![swapped],
            "old directory gone"
        );
        assert!(scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)).is_empty());
        Ok(())
    }

    #[test]
    fn truncate_abort_restores_every_row_and_removes_the_staged_directory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert_many(
            vec![vec![Value::Int4(1)], vec![Value::Int4(2)]],
            &tm.context(loader, CommandId::FIRST),
        )?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let truncater = tm.allocate_xid();
        table.truncate(&tm.context(truncater, CommandId::FIRST))?;
        tm.abort(truncater);
        assert_eq!(finish(&table, truncater, false)?, None);

        assert_eq!(table.relfilenode(), 1);
        assert_eq!(fragment_dirs(dir.path())?, vec![1], "staged directory gone");
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2]
        );
        Ok(())
    }

    /// Invariant P1: a post-TRUNCATE insert must stamp the *staged* directory's
    /// relfilenode into its footer, or reading it back reports valid bytes as
    /// corrupt — including through a freshly opened handle after a restart.
    #[test]
    fn post_truncate_fragments_carry_the_staged_relfilenode() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        table.insert(vec![Value::Int4(7)], &tm.context(xid, CommandId(1)))?;
        // Visible to its own transaction from the staged directory, before commit.
        assert_eq!(scan_values(&table, &tm.context(xid, CommandId(2))), vec![7]);
        tm.commit(xid)?;
        let swapped = finish(&table, xid, true)?.ok_or_else(|| anyhow::anyhow!("missing swap"))?;

        let file = parquet_files(dir.path(), swapped)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing promoted fragment"))?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&file)?)?;
        let stamped = reader
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .and_then(|kv| {
                kv.iter()
                    .find(|item| item.key == super::META_REL)
                    .and_then(|item| item.value.clone())
            })
            .ok_or_else(|| anyhow::anyhow!("fragment has no relfilenode metadata"))?;
        assert_eq!(stamped, swapped.to_string());

        // A fresh handle over the swapped directory reads the same row back.
        let reopened = open_table(dir.path(), swapped, schema("t", &[PgType::Int4]), wal)?;
        assert_eq!(
            scan_values(&reopened, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![7]
        );
        Ok(())
    }

    /// Invariant P2, abort half: the block counter must return to what the
    /// transaction found, or a later insert re-issues a block an existing fragment
    /// in the surviving directory already owns — duplicate TIDs.
    #[test]
    fn aborted_truncate_restores_the_block_counter() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        table.insert(vec![Value::Int4(2)], &tm.context(loader, CommandId(1)))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let truncater = tm.allocate_xid();
        table.truncate(&tm.context(truncater, CommandId::FIRST))?;
        // Two TRUNCATEs in one transaction must still restore the FIRST counter.
        table.truncate(&tm.context(truncater, CommandId(1)))?;
        tm.abort(truncater);
        finish(&table, truncater, false)?;

        let writer = tm.allocate_xid();
        let tids = table.insert(vec![Value::Int4(3)], &tm.context(writer, CommandId::FIRST))?;
        assert_eq!(tids, Tid::new(3, 1), "block 1 and 2 are still occupied");
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2, 3]
        );
        assert_eq!(fragment_dirs(dir.path())?, vec![1]);
        Ok(())
    }

    /// Invariant P2, commit half: the counter must NOT restart, or the next insert
    /// collides with the fragments the truncating transaction itself wrote.
    #[test]
    fn committed_truncate_keeps_the_counter_its_own_inserts_advanced() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(
            table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId(1)))?,
            Tid::new(1, 1)
        );
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let writer = tm.allocate_xid();
        assert_eq!(
            table.insert(vec![Value::Int4(2)], &tm.context(writer, CommandId::FIRST))?,
            Tid::new(2, 1),
            "the post-truncate insert already owns block 1"
        );
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2]
        );
        Ok(())
    }

    /// `INSERT; TRUNCATE; ROLLBACK`: the pending fragments the transaction staged
    /// *before* its TRUNCATE live in the surviving directory and must be unlinked
    /// there, not promoted.
    #[test]
    fn aborted_insert_then_truncate_unlinks_the_pre_truncate_fragments() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(2)], &tm.context(xid, CommandId::FIRST))?;
        table.truncate(&tm.context(xid, CommandId(1)))?;
        tm.abort(xid);
        finish(&table, xid, false)?;

        assert_eq!(fragment_dirs(dir.path())?, vec![1]);
        assert_eq!(
            parquet_files(dir.path(), 1)?.len(),
            1,
            "only the committed loader's fragment remains"
        );
        assert_eq!(
            std::fs::read_dir(dir.path().join("parquet").join("1"))?.count(),
            1,
            "the aborted transaction's .pending fragment was unlinked"
        );
        Ok(())
    }

    /// A superseded staged directory is reclaimed as soon as the next TRUNCATE in
    /// the same transaction replaces it.
    #[test]
    fn double_truncate_in_one_transaction_reclaims_the_superseded_directory() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        let first_staged = fragment_dirs(dir.path())?;
        assert_eq!(first_staged.len(), 2, "live plus staged");
        table.truncate(&tm.context(xid, CommandId(1)))?;
        let second_staged = fragment_dirs(dir.path())?;
        assert_eq!(second_staged.len(), 2, "the superseded directory is gone");
        assert_ne!(first_staged, second_staged);
        tm.commit(xid)?;
        let swapped = finish(&table, xid, true)?.ok_or_else(|| anyhow::anyhow!("missing swap"))?;
        assert_eq!(fragment_dirs(dir.path())?, vec![swapped]);
        Ok(())
    }

    #[test]
    fn truncate_resets_the_analyze_cache_on_commit_only() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;
        table.set_analyzed(7, 1.0);

        let rolled_back = tm.allocate_xid();
        table.truncate(&tm.context(rolled_back, CommandId::FIRST))?;
        assert_eq!(table.statistics().relpages, 7, "still cached while staged");
        tm.abort(rolled_back);
        finish(&table, rolled_back, false)?;
        assert_eq!(table.statistics().relpages, 7, "an abort changes nothing");

        let committed = tm.allocate_xid();
        table.truncate(&tm.context(committed, CommandId::FIRST))?;
        tm.commit(committed)?;
        finish(&table, committed, true)?;
        let stats = table.statistics();
        assert!(
            !stats.analyzed,
            "back to never-analyzed, as PostgreSQL reports"
        );
        assert_eq!(stats.relpages, 0);
        Ok(())
    }

    /// A measurement taken inside a transaction covers that transaction's own
    /// not-yet-committed fragments (the same rows it counts). If the transaction
    /// rolls back, those fragments are unlinked, so the cached result describes
    /// bytes that no longer exist and must be dropped.
    #[test]
    fn a_rollback_discards_statistics_measured_over_its_own_fragments() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;

        let xid = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(xid, CommandId::FIRST))?;
        let (relpages, reltuples) = table.measure(&tm.context(xid, CommandId(1)))?;
        assert!(relpages > 0, "its own pending fragment is measured");
        assert_eq!(reltuples, 1.0);
        table.set_analyzed_by(xid, relpages, reltuples);

        tm.abort(xid);
        finish(&table, xid, false)?;
        let stats = table.statistics();
        assert!(
            !stats.analyzed,
            "the rolled-back measurement must not survive: {stats:?}"
        );
        assert_eq!(stats.relpages, 0);
        Ok(())
    }

    /// The same rule for a rolled-back TRUNCATE: a measurement the transaction took
    /// of its staged (empty) directory goes with it, while one taken before the
    /// TRUNCATE still describes the directory that survives.
    #[test]
    fn a_rolled_back_truncate_discards_only_its_own_measurement() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // Measured before any TRUNCATE: still valid after one rolls back.
        let (relpages, reltuples) = table.measure(&tm.context(Xid::INVALID, CommandId::FIRST))?;
        table.set_analyzed(relpages, reltuples);
        let rolled_back = tm.allocate_xid();
        table.truncate(&tm.context(rolled_back, CommandId::FIRST))?;
        tm.abort(rolled_back);
        finish(&table, rolled_back, false)?;
        let stats = table.statistics();
        assert!(stats.analyzed, "an older measurement survives an abort");
        assert_eq!((stats.relpages, stats.reltuples), (relpages, reltuples));

        // Measured by the truncating transaction, against the staged empty
        // directory: discarded when that directory is.
        let second = tm.allocate_xid();
        table.truncate(&tm.context(second, CommandId::FIRST))?;
        let (staged_pages, staged_rows) = table.measure(&tm.context(second, CommandId(1)))?;
        assert_eq!((staged_pages, staged_rows), (0, 0.0));
        table.set_analyzed_by(second, staged_pages, staged_rows);
        tm.abort(second);
        finish(&table, second, false)?;
        assert!(
            !table.statistics().analyzed,
            "a measurement of the discarded directory must not outlive it"
        );
        Ok(())
    }

    /// ANALYZE inside an uncommitted TRUNCATE must measure ONE directory: pairing
    /// the staged directory's row count with the old one's page count would persist
    /// statistics describing a relation that never existed.
    #[test]
    fn measure_inside_an_uncommitted_truncate_sees_only_the_staged_directory() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;
        let (loaded_pages, loaded_rows) =
            table.measure(&tm.context(Xid::INVALID, CommandId::FIRST))?;
        assert!(loaded_pages > 0);
        assert_eq!(loaded_rows, 1.0);

        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(table.measure(&tm.context(xid, CommandId(1)))?, (0, 0.0));
        // And once the TRUNCATE rolls back, the old measurement is what everyone
        // (including another session, which may now take the lock) sees again.
        tm.abort(xid);
        finish(&table, xid, false)?;
        assert_eq!(
            table.measure(&tm.context(Xid::INVALID, CommandId::FIRST))?,
            (loaded_pages, loaded_rows)
        );
        Ok(())
    }

    #[test]
    fn drop_storage_removes_the_staged_directory_too() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let xid = tm.allocate_xid();
        table.truncate(&tm.context(xid, CommandId::FIRST))?;
        assert_eq!(fragment_dirs(dir.path())?.len(), 2);
        table.drop_storage()?;
        assert!(fragment_dirs(dir.path())?.is_empty());
        Ok(())
    }

    /// The same owner may TRUNCATE a table it is already scanning (lock upgrade),
    /// while another owner's in-flight scan blocks the TRUNCATE until it finishes.
    #[test]
    fn truncate_upgrades_over_its_own_scan_and_waits_for_a_foreign_one() -> anyhow::Result<()> {
        use crabgresql_txn::LockOwner;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = Arc::new(open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?);
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // Same owner: an open scan does not self-deadlock the TRUNCATE.
        let own = tm.allocate_xid();
        let mut own_ctx = tm.context(own, CommandId::FIRST);
        own_ctx.lock_owner = LockOwner(42);
        let cursor = table.scan(&own_ctx, &ColumnProjection::All);
        table.truncate(&own_ctx)?;
        drop(cursor);
        tm.abort(own);
        finish(&table, own, false)?;

        // Foreign owner: the TRUNCATE must wait for the scan to be dropped.
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        let cursor = table.scan(&reader_ctx, &ColumnProjection::All);
        let truncater = tm.allocate_xid();
        let mut truncate_ctx = tm.context(truncater, CommandId::FIRST);
        truncate_ctx.lock_owner = LockOwner(8);
        let (tx, rx) = mpsc::channel();
        let worker = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || {
                let outcome = table.truncate(&truncate_ctx);
                tx.send(()).expect("send");
                outcome
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "TRUNCATE must wait for a foreign owner's open scan"
        );
        drop(cursor);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("TRUNCATE must proceed once the scan is dropped");
        worker.join().expect("worker panicked")?;
        Ok(())
    }

    /// Invariant P1 under contention: a reader that parks in `acquire_shared` while
    /// a TRUNCATE holds the table must read the directory that exists when it is
    /// finally granted the hold — not the one it saw before waiting. Resolving the
    /// relfilenode before the lock made a plain scan of healthy data fail with
    /// `CorruptData`, because the footer stamp belongs to the new generation.
    #[test]
    fn a_scan_that_waits_for_a_truncate_reads_the_swapped_in_directory() -> anyhow::Result<()> {
        use crabgresql_txn::LockOwner;
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = Arc::new(open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?);
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // The truncater commits (so the CLOG already says committed and its rows are
        // MVCC-visible) but has not been finalized yet, so it still holds the table.
        let truncater = tm.allocate_xid();
        let mut truncate_ctx = tm.context(truncater, CommandId::FIRST);
        truncate_ctx.lock_owner = LockOwner(8);
        table.truncate(&truncate_ctx)?;
        table.insert(vec![Value::Int4(7)], &{
            let mut ctx = tm.context(truncater, CommandId(1));
            ctx.lock_owner = LockOwner(8);
            ctx
        })?;
        tm.commit(truncater)?;

        // A foreign reader whose snapshot sees the committed truncater, parked on the
        // exclusive hold.
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        let (tx, rx) = mpsc::channel();
        let reader = {
            let table = Arc::clone(&table);
            std::thread::spawn(move || {
                let rows: Vec<Result<(Tid, Tuple), StorageError>> =
                    table.scan(&reader_ctx, &ColumnProjection::All).collect();
                tx.send(()).expect("send");
                rows
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "the reader must wait for the TRUNCATE's exclusive hold"
        );
        // Finalize: the directory the reader looked at before waiting is now gone.
        finish(&table, truncater, true)?;
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the reader must proceed once the hold is released");
        let rows = reader.join().expect("reader panicked");
        let values: Vec<i32> = rows
            .into_iter()
            .map(
                |row| match row.expect("a waiting scan must not report corruption").1[0] {
                    Value::Int4(value) => value,
                    ref other => panic!("unexpected value {other:?}"),
                },
            )
            .collect();
        assert_eq!(values, vec![7]);
        Ok(())
    }

    /// A TRUNCATE that fails before staging anything must not keep the exclusive
    /// hold: nothing would ever release it (the transaction has no staged swap for
    /// the finalize path to find), and the table would be unusable for the process
    /// lifetime.
    #[test]
    fn a_truncate_that_fails_to_stage_releases_the_exclusive_hold() -> anyhow::Result<()> {
        use crabgresql_txn::LockOwner;

        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        // Occupy the path the allocator will hand out with a regular file, so
        // `create_dir_all` for the staged directory fails.
        std::fs::write(dir.path().join("parquet").join("1000"), b"not a directory")?;

        let truncater = tm.allocate_xid();
        let mut truncate_ctx = tm.context(truncater, CommandId::FIRST);
        truncate_ctx.lock_owner = LockOwner(8);
        let error = table
            .truncate(&truncate_ctx)
            .expect_err("staging must fail when the directory cannot be created");
        assert!(matches!(error, StorageError::Io(_)), "{error:?}");
        tm.abort(truncater);
        finish(&table, truncater, false)?;

        // Another owner can still read and write: the hold was released.
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        assert!(scan_values(&table, &reader_ctx).is_empty());
        let writer = tm.allocate_xid();
        let mut write_ctx = tm.context(writer, CommandId::FIRST);
        write_ctx.lock_owner = LockOwner(7);
        table.insert(vec![Value::Int4(3)], &write_ctx)?;
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        let mut reader_ctx = tm.context(Xid::INVALID, CommandId::FIRST);
        reader_ctx.lock_owner = LockOwner(7);
        assert_eq!(scan_values(&table, &reader_ctx), vec![3]);
        Ok(())
    }

    /// `rebind`'s fallible steps run before it publishes the new relfilenode: the
    /// caller logs a failure and keeps serving the table, so a half-applied rebind
    /// (new directory, old block counter) would hand out colliding block numbers.
    #[test]
    fn a_failed_rebind_leaves_the_table_on_its_old_directory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("t", &[PgType::Int4]),
            Arc::clone(&wal),
        )?;
        let loader = tm.allocate_xid();
        table.insert(vec![Value::Int4(1)], &tm.context(loader, CommandId::FIRST))?;
        tm.commit(loader)?;
        finish(&table, loader, true)?;

        // A regular file where the rebind target directory should be.
        std::fs::write(dir.path().join("parquet").join("77"), b"not a directory")?;
        table
            .rebind(77)
            .expect_err("rebind must fail when the directory cannot be created");
        assert_eq!(
            table.relfilenode(),
            1,
            "the table stays on its old directory"
        );

        // And the block counter still describes that directory: the next insert does
        // not collide with the existing fragment.
        let writer = tm.allocate_xid();
        let tid = table.insert(vec![Value::Int4(2)], &tm.context(writer, CommandId::FIRST))?;
        assert_eq!(tid, Tid::new(2, 1));
        tm.commit(writer)?;
        finish(&table, writer, true)?;
        assert_eq!(
            scan_values(&table, &tm.context(Xid::INVALID, CommandId::FIRST)),
            vec![1, 2]
        );
        Ok(())
    }

    /// A relation whose columns span the two shapes that matter to a
    /// `ProjectionMask`: plain scalars, and the `Struct`-backed `timetz` /
    /// `interval` that own several *leaf* descriptors under one root.
    fn struct_mixed_table(
        dir: &Path,
        wal: Arc<Wal>,
    ) -> Result<(ParquetTable, Vec<Value>), StorageError> {
        let schema = schema(
            "mixed",
            &[
                PgType::Int4,
                PgType::Interval,
                PgType::Text,
                PgType::TimeTz,
                PgType::Bool,
            ],
        );
        let row = vec![
            Value::Int4(7),
            Value::Interval(Interval {
                months: 14,
                days: -3,
                usec: 777,
            }),
            Value::Text("payload".to_string()),
            Value::TimeTz(TimeTz {
                usec: 45_000_000,
                zone: 3_600,
            }),
            Value::Bool(true),
        ];
        let table = open_table(dir, 1, schema, wal)?;
        Ok((table, row))
    }

    /// Every column outside the projection reads back as `Null`, every one
    /// inside keeps its real value, and the tuple stays as wide as the schema.
    ///
    /// Projecting *around* a `Struct` column (index 1 here) is the part that
    /// would break under a naive positional decode: the batch is dense over the
    /// selected columns, so batch position 1 is schema position 2.
    #[test]
    fn a_projected_scan_fills_only_the_selected_columns() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let (table, row) = struct_mixed_table(dir.path(), Arc::clone(&wal))?;

        let xid = tm.allocate_xid();
        table.insert(row.clone(), &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        let projected: Vec<Tuple> = table
            .scan(&reader, &ColumnProjection::of([0, 2], &table.schema()))
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;

        assert_eq!(
            projected,
            vec![vec![
                row[0].clone(),
                Value::Null,
                row[2].clone(),
                Value::Null,
                Value::Null,
            ]]
        );
        Ok(())
    }

    /// `timetz` and `interval` map to an arrow `Struct`, so they occupy several
    /// leaf descriptors under a single root. Building the mask from *leaf*
    /// indices would select the wrong columns; this pins `roots`.
    #[test]
    fn a_projection_of_only_struct_columns_decodes_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let (table, row) = struct_mixed_table(dir.path(), Arc::clone(&wal))?;

        let xid = tm.allocate_xid();
        table.insert(row.clone(), &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        for column in [1usize, 3] {
            let rows: Vec<Tuple> = table
                .scan(&reader, &ColumnProjection::of([column], &table.schema()))
                .map(|result| result.map(|(_, tuple)| tuple))
                .collect::<Result<_, _>>()?;
            let mut want = vec![Value::Null; row.len()];
            want[column] = row[column].clone();
            assert_eq!(rows, vec![want], "projecting only column {column}");
        }
        Ok(())
    }

    /// The bytes in a fragment are in **Arrow's** epoch, not PostgreSQL's.
    ///
    /// A fragment is a persisted format, so this is a compatibility boundary
    /// rather than an internal detail: fragments written before the conversion
    /// moved out of the per-row decode must still read back correctly. Every
    /// other temporal test round-trips through both directions at once and would
    /// pass just as happily if the shift were dropped or inverted on both sides,
    /// so this is the only place the actual on-disk value is pinned.
    #[test]
    fn a_fragment_stores_temporal_columns_in_the_unix_epoch() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let table = open_table(
            dir.path(),
            1,
            schema("temporal", &[PgType::Date, PgType::Timestamp]),
            Arc::clone(&wal),
        )?;

        // 2000-01-01, PostgreSQL's epoch: day 0 and microsecond 0 to us, and
        // exactly PG_UNIX_EPOCH_DAYS / _MICROS to Arrow.
        let xid = tm.allocate_xid();
        table.insert_many(
            vec![
                vec![Value::Date(0), Value::Timestamp(0)],
                vec![Value::Date(i32::MAX), Value::Timestamp(i64::MIN)],
            ],
            &tm.context(xid, CommandId::FIRST),
        )?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let files = parquet_files(dir.path(), 1)?;
        let mut reader =
            ParquetRecordBatchReaderBuilder::try_new(File::open(&files[0])?)?.build()?;
        let batch = reader
            .next()
            .ok_or_else(|| anyhow::anyhow!("fragment has no batch"))??;

        let dates = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::Date32Array>()
            .ok_or_else(|| anyhow::anyhow!("column 0 is not a Date32"))?;
        let stamps = batch
            .column(1)
            .as_any()
            .downcast_ref::<arrow_array::TimestampMicrosecondArray>()
            .ok_or_else(|| anyhow::anyhow!("column 1 is not a TimestampMicrosecond"))?;

        assert_eq!(dates.value(0), super::PG_UNIX_EPOCH_DAYS);
        assert_eq!(stamps.value(0), super::PG_UNIX_EPOCH_MICROS);
        // The ±infinity sentinels are stored verbatim: shifting them would both
        // overflow and turn infinity into an ordinary instant.
        assert_eq!(dates.value(1), i32::MAX);
        assert_eq!(stamps.value(1), i64::MIN);
        Ok(())
    }

    /// Drain a batch scan back into rows, so it can be compared against the row
    /// scan value for value.
    fn batch_scan_rows(
        table: &ParquetTable,
        txn: &TxnContext,
        projection: &ColumnProjection,
    ) -> Result<Vec<Tuple>, StorageError> {
        let schema = table.schema().clone();
        let positions: Vec<usize> = (0..schema.columns.len()).collect();
        let mut rows = Vec::new();
        for batch in table
            .scan_batches(txn, projection)
            .ok_or_else(|| corrupt("engine reported batch support but returned none"))?
        {
            let batch = batch?;
            for row in 0..batch.num_rows() {
                rows.push(crabgresql_storage_api::arrow::decode_row(
                    &schema, &positions, &batch, row,
                )?);
            }
        }
        Ok(rows)
    }

    /// The batch scan and the row scan are the same scan. Every supported type
    /// is present, so this is also where a temporal column that forgot to leave
    /// the file's epoch shows up — a `Date` would come back shifted by
    /// `PG_UNIX_EPOCH_DAYS` and the comparison against the row scan would fail.
    #[test]
    fn a_batch_scan_yields_exactly_what_the_row_scan_does() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema(
            "types",
            &[
                PgType::Bool,
                PgType::Int4,
                PgType::Numeric,
                PgType::Text,
                PgType::Uuid,
                PgType::Date,
                PgType::Time,
                PgType::TimeTz,
                PgType::Timestamp,
                PgType::TimestampTz,
                PgType::Interval,
            ],
        );
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;
        let row = |n: i32| {
            Ok::<Tuple, anyhow::Error>(vec![
                Value::Bool(n % 2 == 0),
                Value::Int4(n),
                Value::Numeric(Numeric::parse("1234567890.012300")?),
                Value::Text(format!("row {n}")),
                Value::Uuid([n as u8; 16]),
                // Either side of both epochs, plus the ±infinity sentinels.
                Value::Date(n * 4_000 - 8_000),
                Value::Time(12_345_678),
                Value::TimeTz(TimeTz {
                    usec: 45_000_000,
                    zone: 3_600,
                }),
                Value::Timestamp(i64::from(n) * 1_000_000_000 - 2_000_000_000),
                Value::TimestampTz(-987_654_321),
                Value::Interval(Interval {
                    months: 14,
                    days: -3,
                    usec: 777,
                }),
            ])
        };
        let mut expected: Vec<Tuple> = (0..4).map(row).collect::<Result<_, _>>()?;
        // The sentinels are ordinary bit patterns to Arrow; a rebase that did
        // not exempt them would overflow or turn infinity into a date.
        let mut infinities = row(0)?;
        infinities[5] = Value::Date(i32::MAX);
        infinities[8] = Value::Timestamp(i64::MIN);
        expected.push(infinities);
        expected.push(vec![Value::Null; 11]);

        // Two fragments, so the batch scan has to cross a fragment boundary.
        for chunk in expected.chunks(3) {
            let xid = tm.allocate_xid();
            table.insert_many(chunk.to_vec(), &tm.context(xid, CommandId::FIRST))?;
            tm.commit(xid)?;
            finish(&table, xid, true)?;
        }

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        let row_scan: Vec<Tuple> = table
            .scan(&reader, &ColumnProjection::All)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;
        assert_eq!(row_scan, expected);
        assert_eq!(
            batch_scan_rows(&table, &reader, &ColumnProjection::All)?,
            expected
        );
        Ok(())
    }

    /// A projected batch comes back at full width, with the skipped columns
    /// NULL — the same contract the row scan has, so the two still agree.
    #[test]
    fn a_projected_batch_scan_stays_full_width() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let (table, row) = struct_mixed_table(dir.path(), Arc::clone(&wal))?;

        let xid = tm.allocate_xid();
        table.insert_many(vec![row.clone()], &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(&table, xid, true)?;

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        // Project around the `Interval` struct column, the case a naive
        // positional decode gets wrong.
        let projection = ColumnProjection::of([2], &table.schema());
        let batched = batch_scan_rows(&table, &reader, &projection)?;
        let scanned: Vec<Tuple> = table
            .scan(&reader, &projection)
            .map(|result| result.map(|(_, tuple)| tuple))
            .collect::<Result<_, _>>()?;

        assert_eq!(batched.len(), 1);
        assert_eq!(
            batched[0].len(),
            row.len(),
            "width is the schema's, not the projection's"
        );
        assert_eq!(batched[0][2], row[2]);
        assert_eq!(batched, scanned);
        Ok(())
    }

    /// A mask prunes columns, never rows — so the tid sequence, and `fetch`'s
    /// ability to find a row by it, must be identical to an unprojected scan.
    /// Spans several fragments, since the ordinal restarts within each.
    #[test]
    fn a_projected_scan_yields_the_same_tids_as_a_full_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let (table, row) = struct_mixed_table(dir.path(), Arc::clone(&wal))?;

        // Three fragments, two rows each: one `insert_many` per transaction.
        for _ in 0..3 {
            let xid = tm.allocate_xid();
            table.insert_many(
                vec![row.clone(), row.clone()],
                &tm.context(xid, CommandId::FIRST),
            )?;
            tm.commit(xid)?;
            finish(&table, xid, true)?;
        }

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        let tids = |projection: &ColumnProjection| -> Result<Vec<Tid>, StorageError> {
            table
                .scan(&reader, projection)
                .map(|result| result.map(|(tid, _)| tid))
                .collect()
        };
        let full = tids(&ColumnProjection::All)?;
        assert_eq!(full.len(), 6);
        assert_eq!(tids(&ColumnProjection::of([2], &table.schema()))?, full);
        // The empty set is the `count(*)` shape, normalized to one column.
        assert_eq!(tids(&ColumnProjection::of([], &table.schema()))?, full);

        // `fetch` still resolves each tid to the whole row.
        for tid in full {
            assert_eq!(table.fetch(tid, &reader)?, Some(row.clone()));
        }
        Ok(())
    }

    /// [`schema`] plus a layout sort key over `key`, ascending / NULLS LAST —
    /// the shape a bare `ORDER BY (columns)` produces.
    fn sorted_schema(name: &str, types: &[PgType], key: &[usize]) -> TableSchema {
        let mut schema = schema(name, types);
        schema.sort_key = key
            .iter()
            .map(|column| IndexKey {
                column: *column,
                descending: false,
                nulls_first: false,
            })
            .collect();
        schema
    }

    /// Every row of `rel`'s fragments, in the order the files store them.
    fn stored_rows(dir: &Path, rel: u32, schema: &TableSchema) -> anyhow::Result<Vec<Tuple>> {
        let positions: Vec<usize> = (0..schema.columns.len()).collect();
        let mut rows = Vec::new();
        for file in parquet_files(dir, rel)? {
            let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&file)?)?.build()?;
            for batch in reader {
                let batch = super::from_file_epoch(&batch?)?;
                for row in 0..batch.num_rows() {
                    rows.push(decode_row(schema, &positions, &batch, row)?);
                }
            }
        }
        Ok(rows)
    }

    /// The sort key a fragment declares, as `(leaf, descending, nulls_first)`,
    /// or `None` when it claims no order at all.
    fn declared_sort(path: &Path) -> anyhow::Result<Option<Vec<(i32, bool, bool)>>> {
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
        Ok(reader
            .metadata()
            .row_group(0)
            .sorting_columns()
            .map(|columns| {
                columns
                    .iter()
                    .map(|column| (column.column_idx, column.descending, column.nulls_first))
                    .collect()
            }))
    }

    /// A `numeric` column lands in the Parquet physical type its precision
    /// asks for, which is where the space is won: `INT32` up to 9 digits,
    /// `INT64` to 18, then a fixed-length byte array sized by the precision.
    /// The text encoding this replaced was a `BYTE_ARRAY` for every width.
    #[test]
    fn a_numeric_column_lands_in_the_physical_type_its_precision_asks_for() -> anyhow::Result<()> {
        use parquet::basic::Type as Physical;

        for (precision, scale, expected, length) in [
            (9i32, 2i32, Physical::INT32, 0),
            (18, 2, Physical::INT64, 0),
            (38, 2, Physical::FIXED_LEN_BYTE_ARRAY, 16),
            (76, 2, Physical::FIXED_LEN_BYTE_ARRAY, 32),
        ] {
            let dir = tempfile::tempdir()?;
            let wal = Arc::new(Wal::open(dir.path())?);
            let tm = manager(&wal);
            let mut schema = schema("phys", &[PgType::Numeric]);
            schema.columns[0].typmod = Numeric::pack_typmod(precision, scale);
            let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;
            insert_committed(
                &table,
                &tm,
                vec![vec![Value::Numeric(
                    Numeric::parse("1.25")?.apply_typmod(precision, scale)?,
                )]],
            )?;

            let file = parquet_files(dir.path(), 1)?
                .pop()
                .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
            let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(&file)?)?;
            let column = reader.metadata().file_metadata().schema_descr().column(0);
            assert_eq!(
                column.physical_type(),
                expected,
                "numeric({precision},{scale})"
            );
            assert_eq!(
                column.type_length().max(0),
                length,
                "numeric({precision},{scale}) byte length"
            );
        }
        Ok(())
    }

    /// Insert `rows` in one transaction and commit it.
    fn insert_committed(
        table: &ParquetTable,
        tm: &TransactionManager,
        rows: Vec<Tuple>,
    ) -> anyhow::Result<Vec<Tid>> {
        let xid = tm.allocate_xid();
        let tids = table.insert_many(rows, &tm.context(xid, CommandId::FIRST))?;
        tm.commit(xid)?;
        finish(table, xid, true)?;
        Ok(tids)
    }

    #[test]
    fn a_fragment_stores_its_rows_in_the_layout_sort_key_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = sorted_schema("sorted", &[PgType::Int4, PgType::Text], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [5, 1, 4, 2, 3]
            .into_iter()
            .map(|n| vec![Value::Int4(n), Value::Text(format!("row{n}"))])
            .collect();
        insert_committed(&table, &tm, rows)?;

        let stored = stored_rows(dir.path(), 1, &schema)?;
        let keys: Vec<Value> = stored.into_iter().map(|row| row[0].clone()).collect();
        assert_eq!(
            keys,
            (1..=5).map(Value::Int4).collect::<Vec<_>>(),
            "the file must hold the rows in key order, not insertion order"
        );

        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(0, false, false)]));
        Ok(())
    }

    #[test]
    fn a_sorted_insert_returns_tids_that_still_name_their_own_rows() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = sorted_schema("sorted", &[PgType::Int4, PgType::Text], &[0]);
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;

        // The caller indexes the returned tids by *input* position, so a
        // permutation applied in the wrong direction shows up here and nowhere
        // else: the rows would all be present and all be reachable, just under
        // each other's addresses.
        let rows: Vec<Tuple> = [7, 3, 9, 1, 5, 2]
            .into_iter()
            .map(|n| vec![Value::Int4(n), Value::Text(format!("row{n}"))])
            .collect();
        let tids = insert_committed(&table, &tm, rows.clone())?;

        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        for (tid, row) in tids.iter().zip(&rows) {
            assert_eq!(table.fetch(*tid, &reader)?.as_ref(), Some(row));
        }
        Ok(())
    }

    #[test]
    fn a_sorted_insert_spanning_fragments_does_not_interleave_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = sorted_schema("wide", &[PgType::Int4], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        // Descending input across the fragment boundary: sorting each chunk on
        // its own would produce two sorted files whose ranges cover everything,
        // which prunes exactly as badly as no sort at all.
        let rows: Vec<Tuple> = (0..super::MAX_FRAGMENT_ROWS as i32 + 100)
            .rev()
            .map(|n| vec![Value::Int4(n)])
            .collect();
        let tids = insert_committed(&table, &tm, rows.clone())?;

        let files = parquet_files(dir.path(), 1)?;
        assert_eq!(files.len(), 2);
        let mut previous_max: Option<i32> = None;
        for file in &files {
            let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(file)?)?.build()?;
            let mut min = i32::MAX;
            let mut max = i32::MIN;
            for batch in reader {
                let batch = batch?;
                let values = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<arrow_array::Int32Array>()
                    .ok_or_else(|| anyhow::anyhow!("column 0 is not an Int32"))?;
                min = min.min(values.iter().flatten().min().unwrap_or(i32::MAX));
                max = max.max(values.iter().flatten().max().unwrap_or(i32::MIN));
            }
            if let Some(previous) = previous_max {
                assert!(previous <= min, "fragment key ranges overlap");
            }
            previous_max = Some(max);
        }

        // The tid permutation has to survive the fragment split too.
        let reader = tm.context(Xid::INVALID, CommandId::FIRST);
        for index in [0, 1, super::MAX_FRAGMENT_ROWS - 1, rows.len() - 1] {
            assert_eq!(
                table.fetch(tids[index], &reader)?.as_ref(),
                Some(&rows[index])
            );
        }
        Ok(())
    }

    #[test]
    fn a_relation_with_no_sort_key_keeps_insertion_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema("unsorted", &[PgType::Int4]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [5, 1, 4]
            .into_iter()
            .map(|n| vec![Value::Int4(n)])
            .collect();
        insert_committed(&table, &tm, rows.clone())?;

        assert_eq!(stored_rows(dir.path(), 1, &schema)?, rows);
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, None);
        Ok(())
    }

    #[test]
    fn an_unsortable_sort_key_writes_unsorted_rather_than_failing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        // `interval` maps to a `Struct`, which no ordering kernel accepts, and
        // its PostgreSQL order is by canonical span rather than field by field
        // anyway. DDL rejects such a key today, but a relation created before
        // that check still has to accept writes — unsorted, and saying so.
        let schema = sorted_schema("legacy", &[PgType::Interval], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [2, 1, 3]
            .into_iter()
            .map(|days| {
                vec![Value::Interval(crabgresql_types::Interval {
                    months: 0,
                    days,
                    usec: 0,
                })]
            })
            .collect();
        insert_committed(&table, &tm, rows.clone())?;

        assert_eq!(stored_rows(dir.path(), 1, &schema)?, rows);
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(
            declared_sort(&file)?,
            None,
            "an unsorted fragment must not claim to be clustered"
        );
        Ok(())
    }

    /// A `numeric` sort key orders **numerically**. This is the test that fails
    /// if the column ever goes back to a text encoding: `"10"` and `"100"` both
    /// sort below `"9"` as strings, and the expected order below is the one
    /// string order gets wrong.
    #[test]
    fn a_numeric_sort_key_orders_numerically() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let mut schema = sorted_schema("nums", &[PgType::Numeric], &[0]);
        schema.columns[0].typmod = Numeric::pack_typmod(10, 2);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let numeric = |n: &str| -> anyhow::Result<Tuple> {
            Ok(vec![Value::Numeric(
                Numeric::parse(n)?.apply_typmod(10, 2)?,
            )])
        };
        insert_committed(
            &table,
            &tm,
            vec![numeric("10")?, numeric("9")?, numeric("100")?],
        )?;

        assert_eq!(
            stored_rows(dir.path(), 1, &schema)?,
            vec![numeric("9")?, numeric("10")?, numeric("100")?],
        );
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(0, false, false)]));
        Ok(())
    }

    /// NaN is a legal `numeric` in PostgreSQL and has no decimal image, so the
    /// INSERT that wrote it is refused — not the flush that would have found it
    /// later, with no statement left to blame.
    #[test]
    fn a_nan_is_refused_by_the_insert_not_the_flush() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = schema("nan", &[PgType::Numeric]);
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;

        let xid = tm.allocate_xid();
        let txn = tm.context(xid, CommandId::FIRST);
        let error = table
            .insert(vec![Value::Numeric(Numeric::nan())], &txn)
            .expect_err("NaN has no decimal representation");
        assert!(
            matches!(error, StorageError::NumericFieldOverflow { .. }),
            "unexpected error: {error:?}"
        );
        // Nothing was staged: the refusal happened before any file appeared.
        assert!(parquet_files(dir.path(), 1)?.is_empty());
        Ok(())
    }

    /// A `numeric(p, s)` the format cannot represent is a property of the
    /// declaration, so it is DDL that says no — for both ways it can happen.
    ///
    /// The scale case is the one that bites hardest if missed: PostgreSQL's
    /// `numeric(4,-2)` is ordinary, Arrow carries a negative scale without
    /// complaint, and only the Parquet writer refuses it. That refusal lands in
    /// a flush, which has no statement to fail — so it has to be caught here.
    #[test]
    fn a_typmod_the_format_cannot_represent_is_rejected_by_ddl() {
        let refused = |precision, scale| {
            let mut schema = schema("t", &[PgType::Numeric]);
            schema.columns[0].typmod = Numeric::pack_typmod(precision, scale);
            matches!(
                super::validate_schema(&schema),
                Err(StorageError::UnsupportedType(_))
            )
        };
        assert!(refused(80, 2), "precision past the widest decimal");
        assert!(
            refused(4, -2),
            "negative scale, which Parquet has no DECIMAL for"
        );
        assert!(refused(2, 5), "scale past the precision");

        assert!(!refused(76, 38), "the widest decimal itself");
        assert!(!refused(9, 0), "scale 0 is the boundary, not an exclusion");
        assert!(!refused(9, 9), "scale == precision is legal");

        // No typmod at all is the common case and is stored at the default
        // precision and scale, not refused.
        let bare = schema("bare", &[PgType::Numeric]);
        assert!(super::validate_schema(&bare).is_ok());
    }

    #[test]
    fn a_sorted_fragment_records_its_sorting_columns() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        // `timetz` maps to a two-field `Struct`, so it owns two *leaf*
        // descriptors: the key at column 1 is leaf 2, not leaf 1. A schema of
        // scalars alone would pass with the naive `column_idx = key.column`.
        let schema = sorted_schema("leaves", &[PgType::TimeTz, PgType::Int4], &[1]);
        let table = open_table(dir.path(), 1, schema, Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [2, 1]
            .into_iter()
            .map(|n| {
                vec![
                    Value::TimeTz(TimeTz {
                        usec: 1,
                        zone: 3_600,
                    }),
                    Value::Int4(n),
                ]
            })
            .collect();
        insert_committed(&table, &tm, rows)?;

        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(2, false, false)]));
        Ok(())
    }

    #[test]
    fn a_descending_nulls_first_key_is_honored() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        // Built by hand: the layout `ORDER BY` clause parses bare expressions,
        // so DDL cannot spell a direction, but the key is persisted with both
        // flags and the write path has to honor whatever it finds.
        // TODO: accept ASC/DESC and NULLS FIRST/LAST in the layout ORDER BY
        // clause, so a descending key is reachable through DDL.
        let mut schema = schema("descending", &[PgType::Int4]);
        schema.sort_key = vec![IndexKey {
            column: 0,
            descending: true,
            nulls_first: true,
        }];
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = vec![
            vec![Value::Int4(1)],
            vec![Value::Null],
            vec![Value::Int4(3)],
        ];
        insert_committed(&table, &tm, rows)?;

        assert_eq!(
            stored_rows(dir.path(), 1, &schema)?,
            vec![
                vec![Value::Null],
                vec![Value::Int4(3)],
                vec![Value::Int4(1)],
            ]
        );
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(0, true, true)]));
        Ok(())
    }

    #[test]
    fn a_sorted_fragment_orders_floats_as_postgresql_does() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        let schema = sorted_schema("floats", &[PgType::Float8, PgType::Int4], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        // `-0.0` ties with `0.0` and the two NaN bit patterns tie with each
        // other, so the stability tiebreak decides both — that the *write* path
        // reaches the shared canonicalization is what this pins down.
        let other_nan = f64::from_bits(f64::NAN.to_bits() | 1);
        let rows: Vec<Tuple> = [other_nan, 0.0, f64::NAN, -0.0, -1.0]
            .into_iter()
            .enumerate()
            .map(|(index, value)| vec![Value::Float8(value), Value::Int4(index as i32)])
            .collect();
        insert_committed(&table, &tm, rows)?;

        let tags: Vec<Value> = stored_rows(dir.path(), 1, &schema)?
            .into_iter()
            .map(|row| row[1].clone())
            .collect();
        assert_eq!(
            tags,
            [4, 1, 3, 0, 2].map(Value::Int4).to_vec(),
            "-1.0 < (0.0, -0.0 in input order) < (NaN, NaN in input order)"
        );

        // The fragment still declares itself sorted, and that declaration is
        // honest under the IEEE comparison Parquet defines for DOUBLE: the two
        // zeros compare equal and NaN's placement is left undefined. Only a
        // reader using Arrow's *total* order would call `+0.0, -0.0` a descent,
        // which is why `sorting_columns`' doc spells the caveat out.
        let file = parquet_files(dir.path(), 1)?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("missing fragment"))?;
        assert_eq!(declared_sort(&file)?, Some(vec![(0, false, false)]));
        Ok(())
    }

    #[test]
    fn a_char_key_sorts_by_its_unsigned_byte() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let wal = Arc::new(Wal::open(dir.path())?);
        let tm = manager(&wal);
        // `"char"` is stored as `UInt8` for exactly this reason: a high-bit byte
        // must sort *above* an ASCII one, as PostgreSQL's unsigned comparison
        // says, and would sort below it under a signed encoding.
        let schema = sorted_schema("chars", &[PgType::Char], &[0]);
        let table = open_table(dir.path(), 1, schema.clone(), Arc::clone(&wal))?;

        let rows: Vec<Tuple> = [0xFF, 0x41, 0x00, 0x80]
            .into_iter()
            .map(|byte| vec![Value::Char(byte)])
            .collect();
        insert_committed(&table, &tm, rows)?;

        assert_eq!(
            stored_rows(dir.path(), 1, &schema)?,
            [0x00, 0x41, 0x80, 0xFF]
                .into_iter()
                .map(|byte| vec![Value::Char(byte)])
                .collect::<Vec<_>>()
        );
        Ok(())
    }
}
