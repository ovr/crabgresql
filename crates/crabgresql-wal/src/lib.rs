//! Write-ahead log: a **core service** shared by every durable storage engine.
//!
//! The model follows PostgreSQL's resource-manager (rmgr) design (and the ARIES
//! recovery algorithm, Mohan et al. 1992): the WAL is one ordered byte stream of
//! self-describing records; each record names a resource manager, and each rmgr
//! registers a **redo** handler that knows how to reapply its records to a page
//! during crash recovery. This crate owns the stream (append, group-commit
//! fsync, segment file), the record envelope with a CRC, the rmgr registry, and
//! the redo-only recovery pass. Engines (`crabgresql-pg-engine`) register their
//! record types and redo handlers; the memory engine writes no WAL at all.
//!
//! Clean-room: the durability behavior (write-ahead rule, redo-only recovery, a
//! per-record CRC) is reproduced from the published ARIES algorithm and the
//! PostgreSQL documentation, never from PG's C source.
//!
//! ## The write-ahead (WAL-before-data) contract
//!
//! A data page `P` that a record with end-[`Lsn`] `L` modified stamps `P.lsn =
//! L`. The buffer pool **must not write `P` back to its heap/index file until
//! [`Wal::flushed_lsn`]`() >= L`** — otherwise a crash could leave a data change
//! on disk with no WAL record to recover it. The engine enforces this by calling
//! [`Wal::flush`]`(P.lsn)` before flushing any dirty page. This crate only
//! guarantees that `append` hands back a monotonic end-LSN and that
//! `flush`/`flushed_lsn` are honest about what is on stable storage.

mod aligned;
mod ckpt;
mod control;
mod fsutil;
mod page;
mod reader;
mod record;
mod recovery;
mod rmgr;
mod segment;
mod wal;

pub use ckpt::{CHECKPOINT_ONLINE, CHECKPOINT_SHUTDOWN, Checkpoint};
pub use control::{ControlFile, control_path, read_control, write_control};
pub use fsutil::sync_dir;
pub use page::{
    PageHeader, XLOG_BLCKSZ, XLP_FIRST_IS_CONTRECORD, XLP_PAGE_HEADER_SIZE, XLP_USABLE, advance,
    advance_lsn, first_usable, is_record_position, page_offset, page_start,
};
pub use reader::{StopReason, WalReader, end_of_wal};
pub use record::{Lsn, LsnRange, WalError, WalRecord};
pub use recovery::{RecoveryResult, recover};
pub use rmgr::{RedoContext, RmgrId, RmgrRedo, RmgrRegistry, XACT_ABORT, XACT_COMMIT};
pub use segment::{
    WAL_SEG_SIZE, parse_segment_name, seg_offset, segment_bounds, segment_name, segment_path,
    segment_start, segno_of, wal_dir,
};
pub use wal::{CheckpointDelay, Wal, wal_path};
