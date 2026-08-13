//! One immutable Parquet file: its name, its footer identity, and the reader
//! that opens it.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::RecordBatchReader;

use crabgresql_storage_api::{ColumnProjection, MAX_PHYSICAL_BLOCK, StorageError, TableSchema};
use crabgresql_txn::{CommandId, Infomask, TupleHeader, TxnContext, Xid};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::file::metadata::KeyValue;

use crate::error::{corrupt, io_error};
use crate::schema::schema_identity;

pub(crate) const FORMAT_VERSION: &str = "1";
pub(crate) const MAX_FRAGMENT_ROWS: usize = u16::MAX as usize;

pub(crate) const META_VERSION: &str = "crabgresql.format_version";
pub(crate) const META_REL: &str = "crabgresql.relfilenode";
pub(crate) const META_XMIN: &str = "crabgresql.xmin";
pub(crate) const META_CMIN: &str = "crabgresql.cmin";
pub(crate) const META_SCHEMA: &str = "crabgresql.schema";

#[derive(Clone, Debug)]
pub(crate) struct Fragment {
    pub(crate) path: PathBuf,
    pub(crate) block: u32,
    /// The transaction that wrote the fragment. Reconciliation keys off this even
    /// for a frozen fragment, whose *visibility* is decided by [`Fragment::frozen`].
    pub(crate) xid: Xid,
    pub(crate) cid: CommandId,
    /// Written by `COPY … FREEZE`: the fragment's rows are visible to every
    /// snapshot, so [`header`] reports `xmin = Xid::FROZEN` for it.
    pub(crate) frozen: bool,
    pub(crate) pending: bool,
}

impl Fragment {
    /// The name this fragment takes once its transaction commits: the same file
    /// with the `.pending` suffix stripped. `None` for an already-promoted one.
    pub(crate) fn promoted_path(&self) -> Option<PathBuf> {
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
pub(crate) fn open_fragment_file(fragment: &Fragment) -> Result<File, StorageError> {
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

pub(crate) fn parse_fragment(path: PathBuf) -> Result<Option<Fragment>, StorageError> {
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
pub(crate) fn fragments(dir: &Path) -> Result<Vec<Fragment>, StorageError> {
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
/// what [`crate::ParquetTable::reconcile_pending_in`] matches on to promote or unlink the
/// file; the `-f` segment is what marks its rows frozen for readers. Encoding
/// `Xid::FROZEN` as the name's XID instead would strand the fragment `.pending`
/// forever — visible, since frozen rows ignore that suffix, and never unlinked on
/// abort.
pub(crate) fn fragment_base(block: u32, txn: &TxnContext) -> String {
    format!(
        "{block:08x}-{}-{}{}",
        txn.xid.0,
        txn.cid.0,
        if txn.freeze_inserts { "-f" } else { "" }
    )
}

pub(crate) fn header(fragment: &Fragment) -> TupleHeader {
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

pub(crate) fn metadata_map(metadata: Option<&Vec<KeyValue>>) -> HashMap<&str, &str> {
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
/// listed from* — [`crate::ParquetTable::effective_rel`] for a reader inside a
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
pub(crate) fn open_reader(
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

pub(crate) fn sync_dir(dir: &Path) -> Result<(), StorageError> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|error| io_error("fsync Parquet table directory", error))
}

/// The first block number no fragment in `dir` owns.
pub(crate) fn next_block_in(dir: &Path) -> Result<u32, StorageError> {
    Ok(fragments(dir)?
        .into_iter()
        .map(|fragment| fragment.block)
        .max()
        .unwrap_or(0)
        .saturating_add(1))
}

/// `fragments` measured in 8 KB pages, so `relpages` is comparable to a heap
/// relation's.
pub(crate) fn relpages_of(fragments: &[Fragment]) -> u32 {
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
pub(crate) fn relpages_in(dir: &Path) -> Result<u32, StorageError> {
    let committed: Vec<Fragment> = fragments(dir)?
        .into_iter()
        .filter(|fragment| !fragment.pending)
        .collect();
    Ok(relpages_of(&committed))
}

/// Remove a fragment directory; an already-absent one is success. Every caller is
/// reclaiming storage that may have been reclaimed by a previous attempt, by a
/// crash-time sweep, or never created at all.
pub(crate) fn remove_dir_all_ok(dir: &Path) -> Result<(), StorageError> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("remove Parquet fragment directory", error)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crabgresql_storage_api::StorageError;
    use crabgresql_txn::{CommandId, Xid};

    use super::{header, parse_fragment};

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
}
