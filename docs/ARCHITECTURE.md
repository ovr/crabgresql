# CrabgreSQL — Architecture

> A PostgreSQL-compatible DBMS written in Rust — a general-purpose PostgreSQL
> replacement. The goal is compatibility at the level of the SQL dialect, the
> wire protocol, the system catalogs, and transaction isolation semantics.
> Parity target: **PostgreSQL 19**. Single-node until M5; distributed comes
> later as a separate layer.

## 0. What "compatibility" means

"100% compatibility" is a ladder, not a binary flag. We define levels and climb
them bottom-up:

| Level | What it includes | Who starts working |
|---|---|---|
| **L1 — Protocol & dialect** | pgwire v3, SCRAM auth, simple/extended query, types with their text/binary encodings, SQLSTATE error codes, `pg_catalog`/`information_schema` | psql, drivers (libpq, JDBC, npgsql, tokio-postgres), ORMs, BI tools |
| **L2 — Transactional semantics** | MVCC, all 4 isolation levels with PG semantics, locking (row/table/advisory), `SELECT FOR UPDATE/SHARE`, deadlock detection | Real OLTP applications |
| **L3 — Server-side logic** | PL/pgSQL, triggers, sequences, views/materialized views, LISTEN/NOTIFY, cursors, prepared statements, COPY | Applications with logic in the database, migration tools (Flyway, sqitch) |
| **L4 — Ecosystem** | Logical replication (protocol), FDW, extension API | CDC (Debezium), complex deployments |

The PostgreSQL C ABI for extensions (actual `CREATE EXTENSION` loading `.so`
files) is unattainable on a Rust core — instead we provide our own extension
API plus reimplementations of popular extensions (uuid-ossp, pgcrypto,
pg_stat_statements, later pgvector).

## 1. Key architectural decisions

### 1.1 Parser: sqlparser-rs (PostgreSQL dialect)

We use [`sqlparser`](https://github.com/apache/datafusion-sqlparser-rs)
(Apache, the core of the DataFusion ecosystem) with `PostgreSqlDialect`.

- ✅ pure Rust, no C dependencies or unsafe FFI;
- ✅ an active upstream under the Apache/DataFusion umbrella — gaps in PG
  grammar coverage are closed with upstream PRs (the dialect is extensible);
- ✅ a convenient typed AST — semantic analysis is easier to write than over
  raw PG parse nodes;
- ❌ the grammar does not match PG "by construction" — coverage is proven by
  differential testing: the PG regression-test corpus is run through the
  parser, and every divergence becomes an issue/PR against sqlparser or a fix
  in our wrapper.

The **Binder/Analyzer** layer turns the AST into a typed logical plan,
reproducing the semantics of PG's `parse_analyze` (name resolution, type
inference, operator overload resolution).

sqlparser has no PL/pgSQL support — we write a separate parser for it in
`crabgresql-plpgsql` (the PL/pgSQL grammar is compact and PG stores function
bodies as strings, so this is an isolated M4-level task).

### 1.2 Execution model: Volcano first, vectorization later

PostgreSQL semantics are tied to row-at-a-time execution: the invocation order
of volatile functions, side effects, cursors (`FETCH`), `LIMIT` with early
termination, per-row triggers. Therefore:

- **v1**: a classic Volcano/iterator executor — semantically identical to PG.
- **v2**: vectorized fast paths for read-only plans without volatile functions
  (morally like JIT in PG: enabled only when it is safe).

### 1.3 Storage: pluggable engines (storage engine API)

Storage is not a single implementation but an **extension point**. The core
defines traits in `crabgresql-storage-api` (analogous to the Table AM /
Index AM API from PG 12+); concrete engines are separate crates. The first two:

| Crate | Role |
|---|---|
| `crabgresql-pg-engine` | The engine: durable heap (PG semantics 1:1), plus RAM-backed **memory tables** (`UNLOGGED`/`TEMP`) that skip the WAL |
| `crabgresql-parquet-engine` | Managed, permanent, append-only Parquet tables selected explicitly with `USING parquet` |

API boundaries (`crabgresql-storage-api`):

```rust
trait TableEngine {           // factory: CREATE TABLE ... USING <engine>
    fn create_table(...) -> Box<dyn TableAm>;
}
type TupleStream = Box<dyn Iterator<Item = Result<(Tid, Tuple), StorageError>> + Send>;
trait TableAm {               // scans and modifications, transaction-context-aware
    fn scan(&self, txn: &TxnContext) -> TupleStream;
    fn fetch(&self, tid: Tid, txn: &TxnContext) -> Result<Option<Tuple>>;
    fn insert(&self, tuple, txn) -> Result<Tid>;
    fn insert_many(&self, tuples, txn) -> Result<Vec<Tid>>;
    fn update(&self, tid, tuple, txn) -> Result<UpdateResult>;  // conflict info for EvalPlanQual
    fn delete(&self, tid, txn) -> Result<DeleteResult>;
    fn vacuum(&self, oldest: Xid, clog: &Clog);         // GC of dead versions
}
trait IndexAm { ... }         // insert / scan(range) / bulk build
```

Contract with the core:

- **MVCC is the engine's job; snapshots and XIDs are the core's job.** The core
  (crabgresql-txn) issues snapshots and transaction statuses (CLOG); the engine
  stores versions and answers visibility questions. This way the in-memory
  engine can store versions however it likes (e.g. in-memory version chains)
  while pg-engine keeps them in tuple headers.
- **WAL is a core service** (like resource managers in PG): an engine registers
  its record types and redo handlers. A **memory table** (`UNLOGGED`/`TEMP`)
  simply does not write WAL — its pages live in RAM. The core contract is the
  **write-ahead rule**: a dirty data page stamped with LSN `L` may not be written
  back to its file until the WAL is flushed up to `L` (`Wal::flushed_lsn() >= L`);
  the buffer pool enforces it by flushing WAL before evicting a dirty page, and a
  COMMIT fsyncs the WAL up to its commit record. Recovery is **redo-only** (no
  undo — uncommitted versions are simply invisible and later vacuumed), replaying
  each record through its rmgr's LSN-gated redo handler.
  Implemented in `crabgresql-wal` (+ the heap engine's redo). Deferred from the
  first cut, all correct-but-unbounded/unoptimized without them: a durable SLRU
  CLOG and checkpoint-bounded recovery (recovery currently replays the whole
  WAL), full-page writes for torn-page protection beyond page checksums, and WAL
  segment recycling.
- Syntactically, extensibility is exposed the standard PG way. Plain
  `CREATE TABLE` remains heap; `CREATE TABLE ... USING parquet` explicitly
  selects the columnar method. `default_table_access_method` is unchanged.

**`crabgresql-pg-engine`** — the reference engine, reproducing PG:

- slotted 8 KB heap pages + buffer pool (clock sweep);
- tuple headers with `xmin`, `xmax`, `infomask` — the system columns are real;
- `ctid` = (page, slot) — genuine, not emulated;
- B-tree indexes (Lehman-Yao), later GIN/GiST equivalents;
- WAL: physiological logging, checkpoints, crash recovery (redo-only, like PG —
  no undo needed thanks to MVCC: uncommitted versions are simply dead);
- VACUUM: background collection of dead versions; 64-bit XIDs — see
  "Deliberate deviations" below.

**`crabgresql-parquet-engine`** — managed append-only analytics storage. The
current V1 writes immutable, Snappy-compressed fragments transactionally:
statement-sized batches become fsynced `.pending` files and are promoted on
commit or removed on abort/recovery. It supports transactional TRUNCATE through
a relfilenode-directory swap, but not UPDATE, DELETE, partitioning, compaction,
or external `LOCATION`. Section 2 defines the target buffered, sorted,
chunk-and-partition architecture and explicitly distinguishes it from V1.

Later, the same API enables object-storage engines (serverless) and FDW-like
adapters.

### 1.4 Concurrency: tokio + shared-everything, not process-per-connection

PG's process model is historical baggage. We take:

- **tokio** for the network layer: thousands of connections without an external
  pooler.
- Queries execute on a dedicated CPU pool (executor threads) so they never
  block the reactor.
- A shared buffer pool and a shared lock manager — shared-everything in a
  single process.
- `max_connections` stops being a pain point — that is a feature, not a
  deviation.

### 1.5 Transaction isolation: an exact copy of PG semantics

The PG semantics we reproduce 1:1:

| Level | Implementation in PG (and in ours) |
|---|---|
| `READ UNCOMMITTED` | alias for READ COMMITTED (dirty reads never happen) |
| `READ COMMITTED` | a new snapshot per **statement**; on an UPDATE conflict — `EvalPlanQual` mechanics: re-read the latest row version, re-check the qual, continue |
| `REPEATABLE READ` | Snapshot Isolation: one snapshot per transaction; on a write-write conflict — `ERROR 40001 serialization_failure` |
| `SERIALIZABLE` | **SSI** (Cahill / Ports & Grittner): SIREAD locks (predicate locks on tuple/page/relation), tracking rw-antidependencies, abort on the dangerous structure of two rw edges; `SELECT ... FOR UPDATE` and `SERIALIZABLE READ ONLY DEFERRABLE` as well |

Components:

- **XID manager** + **CLOG** (commit status: in-progress / committed / aborted /
  subcommitted).
- **Snapshot** = `(xmin, xmax, xip_list)` — as in `PGXACT`/`ProcArray`.
- **Visibility check** — the semantics of `HeapTupleSatisfiesMVCC` reproduced
  1:1 (clean-room, see section 7).
- **Lock manager**: table locks (PG's 8 modes with the same conflict matrix),
  row locks via xmax + MultiXact (FOR UPDATE / NO KEY UPDATE / SHARE /
  KEY SHARE), advisory locks, a deadlock detector (waits-for graph, timer like
  `deadlock_timeout`).
- **Subtransactions** (`SAVEPOINT`) — sub-XIDs, rollback to savepoint.

## 2. Parquet table engine

The Parquet table access method is managed database storage, not an exporter
that happens to write `.parquet` files. It owns transaction visibility, WAL
recovery, physical layout, file publication, statistics, and garbage
collection. Clients select it with `CREATE TABLE ... USING parquet`; its
physical organization is not part of PostgreSQL's observable SQL contract.

This section is the **target design**. The current V1 state and implementation
sequence are tracked separately in
[`ROADMAP.md`](../ROADMAP.md#parquet-table-engine), so architectural intent is
not confused with implemented behavior.

The main components and data flow are:

```text
                         ParquetTable (TableAm)
                                  |
                    INSERT/COPY   |   scan/fetch
                                  v
                      +-----------------------+
                      |      BufferTable      |
                      | MVCC rows, RAM, WAL   |
                      | partitioned internally|
                      +----+-------------+----+
                           |             |
                  BUFFER_APPEND WAL      | sealed flush batch
                           v             v
                      core WAL      sort + ChunkWriter
                                          |
                                          v
                              immutable Parquet chunks
                                          |
                                 atomic publication
                                          v
                                 ManifestStore
                                          |
                          scans read pinned generation

BackgroundJobScheduler: Flush, Compact, Repartition, Rewrite, GC, Statistics
```

Every Parquet relation owns one `BufferTable`. `ParquetTable::insert_many`
delegates to it; only a background `Flush` job may invoke `ChunkWriter`.
`ParquetTable::scan` builds a read view from the `BufferTable` and a pinned
manifest generation. This makes the buffer an architectural boundary rather
than an optional batching optimization.

### 2.1 Storage vocabulary and invariants

- A **SQL partition** is PostgreSQL-visible catalog state with a partition
  constraint. It remains a separate leaf relation and is the outermost hard
  routing boundary.
- A **storage partition** is an engine-internal range or bucket inside one
  Parquet leaf relation. It is the unit of buffering, pruning, compaction, and
  parallelism. A storage partition may split or merge online, but may never move
  a row across an SQL partition constraint. "Partition" below means a storage
  partition unless explicitly qualified.
- A **chunk** is exactly one immutable Parquet file. A chunk belongs to one
  table, one storage partition, one schema version, and one partition-spec
  version.
- A **row group** is Parquet's internal encoding and scan unit inside a chunk;
  it is not a chunk and is never independently published.
- A **buffer table** (`BufferTable`) is the mandatory engine-internal,
  RAM-resident table in front of the chunks. It is not a separately addressable
  SQL relation, but it has a schema, contains MVCC row versions, is recoverable
  from WAL, and participates in the same snapshots as durable chunks.
- A **manifest generation** is the immutable list of chunks and storage
  partitions comprising a table at one instant. Readers pin a generation;
  writers publish a new generation atomically.

The following invariants are non-negotiable:

1. INSERT and COPY always enter the relation's buffer table; foreground
   transactions never write directly to a final Parquet file.
2. Acknowledged permanent-table data is present either in durable chunks or in
   WAL that can reconstruct the buffer table.
3. Every chunk is sorted by the table's effective sort key. Equal keys are
   ordered by a hidden, immutable row ID, making the order total and
   deterministic.
4. Chunks never change in place. Flush, compaction, repartitioning, and TRUNCATE
   publish a manifest delta and retire old files.
5. Readers see one manifest generation plus snapshot-visible buffer-table rows.
   Publication can change the physical source of a row, but cannot add, omit,
   or duplicate it in a snapshot.
6. Logical row identity is independent of `(file, row number)`. This is required
   before compaction can be enabled: rewriting a chunk must not invalidate
   executor references or future physical indexes.
7. On-disk format changes are versioned. Existing V1 fragment directories and
   files must remain readable; a background rewrite may migrate them, but an
   upgrade may not require destructive conversion.

### 2.2 Layout metadata and defaults

Each Parquet relation stores a versioned `ParquetLayout` alongside its schema:

```text
partition key        columns/expressions and range or hash strategy
sort key             columns, direction, NULL order, collation version
chunk target         64 MiB of encoded Parquet bytes
compression          Snappy initially; versioned per chunk
partition spec       version plus current bounds/buckets
schema version       physical-to-logical column mapping
buffer limits        per-table soft/hard bytes and maximum row age
```

The default chunk target is **64 MiB compressed**. It is a soft target, not a
correctness boundary: compression ratio is known only while encoding, and one
exceptionally large row or row group may cross it. The writer rotates near the
target and enforces a separate configurable hard limit. Sixty-four MiB is small
enough for useful compaction and parallel scans on local storage while avoiding
the tiny-file behavior of the current per-statement layout. Object-storage
profiles may choose a larger default without changing the file format.

The effective sort key always exists:

1. the partition key is the prefix;
2. an explicitly configured clustering key follows;
3. the hidden logical row ID is the final tie-breaker.

With no partition or clustering key, the row ID alone supplies deterministic
order, although it provides little predicate pruning. Comparisons use
CrabgreSQL/PostgreSQL semantics, including direction, NULL ordering, NaN
behavior, and the recorded collation version. A collation-version change makes
affected chunks candidates for a rewrite rather than silently changing their
claimed order.

The manifest is the source of truth; directory listing is only a recovery and
orphan-GC aid. Each chunk entry contains at least:

```text
chunk ID and path                    state: active or retired
partition/spec/schema versions       row count and encoded byte size
sort-key minimum and maximum         per-column pruning statistics
creation and publication LSNs        MVCC/freeze metadata
checksum and Parquet format version  optional bloom/filter references
```

### 2.3 Buffer table

`BufferTable` is a real storage component with a table-like contract, not the
regular `crabgresql-pg-engine` memory-table implementation. TEMP and UNLOGGED
memory tables deliberately skip WAL; a permanent Parquet buffer must do the
opposite. The two implementations may share Arrow batch utilities, but they
must not share durability semantics.

There is exactly one `BufferTable` per open Parquet relation. Internally it owns
a map of `PartitionId -> PartitionBuffer`, so flush and backpressure can operate
independently on hot partitions while the table retains one transaction and
memory-accounting boundary.

Its logical schema is always derived from the relation schema:

```text
user columns          values supplied by INSERT/COPY
logical row ID        stable identity across flush and compaction
xmin / cmin           MVCC creator and command identity
partition ID          destination under a partition-spec version
schema/spec versions  decoding and routing identity
append WAL LSN        recovery and publication watermark
```

These fields are engine metadata, not SQL-visible columns. A schema change
creates a new buffer-batch schema version; already-buffered batches retain the
version with which they were validated until a flush converts them to the
current physical mapping.

The component exposes four conceptual operations:

```text
append(txn, rows)                  WAL-log and add transaction-owned rows
read(snapshot, manifest_watermark) return visible, not-yet-covered rows
seal(partition, cutoff)            freeze an immutable flush batch
release(published_batch)           reclaim RAM after durable publication
```

A partition buffer moves through explicit states:

```text
Mutable -> Sealed -> Flushing -> Published -> Reclaimable
   ^          |
   +----------+  flush failure returns the batch without losing rows
```

`Mutable` accepts appends. `Sealed` is immutable, so sorting and encoding do not
hold the foreground write lock. `Published` remains readable until the manifest
watermark and all buffer read views prove that the chunk copy is authoritative;
only then is its memory reclaimable. State transitions are idempotent by batch
ID, allowing recovery or a retried job to repeat them safely.

Buffer memory is accounted both per relation and globally by a
`BufferTableManager`. Reaching a table's soft limit seals its largest eligible
partition; the global hard limit applies backpressure to new Parquet writes
until jobs release memory. A maximum row age seals low-volume partitions even
when byte thresholds are not reached.

### 2.4 Write path, MVCC, and WAL

```text
INSERT / COPY
      |
      v
validate + route + allocate logical row IDs
      |
      v
append versioned BUFFER_APPEND WAL record
      |
      v
transaction-owned rows in BufferTable partition
      |
      +---- COMMIT flushes WAL ----> snapshot-visible buffered rows
                                      |
                                      v
                              background sorted flush
                                      |
                                      v
                              chunk + manifest generation
```

`BUFFER_APPEND` carries the table identity, layout/schema versions, XID/CID,
logical row IDs, destination partition, and a checksummed, versioned column
batch. The in-memory representation may use Arrow arrays, but WAL does not
depend on an unstable Rust or Arrow memory layout. As with the heap, appending a
record need not fsync immediately; the transaction's COMMIT record establishes
durability and group commit amortizes the flush.

The buffer table retains transaction metadata. A transaction reads its own
writes; other transactions see buffered rows only when the usual snapshot and
CLOG rules allow them. Abort removes the rows lazily or eagerly. A flush selects
committed rows and leaves in-progress rows buffered. Because one output chunk
may contain rows from several transactions, the V2 physical format stores
per-row MVCC and logical-ID streams as hidden engine metadata (internal columns
or an equivalent sidecar encoding); they are not exposed as user columns. Rows
old enough to be visible to every possible snapshot may be frozen during
compaction.

Every partition buffer tracks its minimum and maximum WAL LSN. A checkpoint may
recycle `BUFFER_APPEND` WAL only after all covered rows are in fsynced chunks
and the manifest generation containing those chunks is durable. Recovery
replays records newer than the durable table watermark, reconstructs the buffer
table, consults CLOG for visibility, and discards aborted rows. Replay is
idempotent by logical row ID, buffer-batch ID, and publication watermark.

### 2.5 Sorted flush and atomic publication

A flush worker seals eligible committed rows from one `PartitionBuffer`, then:

1. sorts them with the effective PostgreSQL comparator and logical row ID
   tie-breaker;
2. writes one or more approximately 64 MiB chunks under unique temporary names;
3. closes and fsyncs each file and its containing directory;
4. appends a WAL-backed manifest delta naming the outputs and covered input
   row IDs/LSNs;
5. atomically publishes the next manifest generation;
6. releases the corresponding buffer-table batch only after publication is
   durable and no pinned buffer read view still references it.

Failure before publication leaves unreferenced temporary files for orphan GC.
Failure after the manifest WAL record but before the manifest file update is
completed by recovery. Failure after publication but before buffer release is
deduplicated by row ID and the manifest's covered-LSN watermark. Therefore a
row is never lost and readers never need to guess whether a file is live.

Sorting happens at flush, not on the foreground transaction path: INSERT
latency remains bounded, while every durable chunk still has a declared order.
The current V1 optimization—one transaction per file with `xmin`/`cmin` in the
footer—remains readable, but cannot be used for multi-transaction compaction
without upgrading to the V2 metadata representation.

### 2.6 Read path and row identity

A scan pins a manifest generation and a transaction snapshot, prunes chunks
using partition bounds and chunk statistics, and reads the remaining row groups
with projection and predicate pushdown where semantics permit. It also scans
snapshot-visible rows through the relation's `BufferTable`.

Moving rows from the buffer table to a chunk while a scan is running must not
create duplicates or omissions. **The implemented mechanism is a flush
transaction, not a watermark.** A flush allocates its own XID `X_f`, writes the
chunk stamped `xmin = X_f`, stamps the copied buffer rows `xmax = X_f`, and
commits — so one CLOG entry decides both halves. `Snapshot::in_progress` reports
`true` for any XID at or above a snapshot's `xmax`, which fixes a reader's
verdict on `X_f` at the moment its snapshot is taken and makes it immune to when
the flush commits. Every reader therefore sees each row either in the buffer or
in the chunk, never both and never neither, whatever the interleaving. Ordinary
`satisfies_mvcc` does the whole job: no covered-LSN watermark, no pinned
generation, and no coordination between the two leaf scans beyond sharing one
`TxnContext`.

This is what lets the two stores be planned as independent `Append` leaves
(§2.1's storage partitions, made visible to the planner through
`TableAm::storage_leaves`) rather than hidden behind one merging scan. Its one
obligation is retention: the buffer copy must survive until `VACUUM` proves no
snapshot still needs it, so a flush may not reclaim eagerly. A covered-LSN
watermark remains the right mechanism for **WAL recycling** — deciding when a
`BUFFER_APPEND` record is no longer needed for recovery — which is a separate
question from read visibility.

Physical chunk order does not imply SQL result order; a query still needs
`ORDER BY`. The layout order exists for range pruning, compression,
merge-friendly compaction, and an executor fast path when the requested SQL
order is compatible.

V1 encodes `Tid` as `(fragment, row offset)` and caps a fragment at 65,535 rows.
That locator is incompatible with background rewrites. Before compaction is
enabled, the storage API must distinguish:

- a stable, engine-owned logical row reference used by fetch and indexes;
- a generation-scoped physical locator used only to find encoded bytes;
- PostgreSQL-facing `ctid`, which remains an opaque compatibility value and is
  not the durable identity of a Parquet row.

Compaction copies the logical row ID unchanged and builds the new generation's
locator mapping. Future indexes point to logical IDs, never filenames or row
ordinals.

### 2.7 Compaction policy

Flush chunks enter level 0 and may have overlapping sort-key ranges. Higher
levels contain non-overlapping ranges within a partition. The initial policy is
a small leveled compactor:

- schedule compaction when a partition has at least eight L0 chunks, excessive
  sub-target chunks, or a configured read-amplification score;
- choose adjacent/overlapping chunks, respecting an I/O budget;
- perform a k-way merge in effective sort order;
- split outputs around the 64 MiB target rather than producing one giant file;
- preserve logical row IDs and required MVCC metadata, freeze rows only when the
  global visibility horizon permits it;
- atomically replace all selected inputs with all outputs in one manifest
  generation.

Chunk count alone is not a reason to rewrite healthy, target-sized,
non-overlapping data. Selection balances read amplification, reclaimable bytes,
sort overlap, and write amplification. Statistics are computed while writing
outputs, so compaction and ANALYZE can share work without making planner
statistics depend on a directory scan.

The compactor validates its input chunk IDs against the current manifest before
publication. If another job has already replaced an input, it abandons its
temporary outputs and retries selection; it never publishes a partial merge.

### 2.8 Background jobs and online repartitioning

The engine provides a shared background-job scheduler with per-table and
per-partition concurrency limits, memory/I/O budgets, cancellation, and
observable progress. Initial job kinds are:

- `Flush` — sealed buffer-table batch to sorted L0 chunks;
- `Compact` — merge/rewrite chunks within a partition;
- `SplitPartition` / `MergePartitions` — change internal partition bounds;
- `RewriteSchemaOrCollation` — migrate a versioned physical layout;
- `GarbageCollect` — delete retired and orphaned files;
- `RefreshStatistics` — publish planner statistics when no rewrite is useful.

The queue itself need not be durable: desired work is rediscovered from buffer
table pressure and manifest state after restart. Only a job's publication is
durable and WAL-backed. Jobs are idempotent, use unique output IDs, and expose
cooperative cancellation points between row groups and output chunks.

Online repartitioning uses partition-spec generations:

1. publish a new spec for routing new writes;
2. keep old-spec chunks readable alongside new-spec chunks;
3. rewrite old chunks into destinations under the new bounds;
4. atomically retire the old spec after every source chunk is replaced;
5. garbage-collect old files only when no reader pins their manifest.

This is how chunks may be redistributed between **storage** partitions. Moving a
row between PostgreSQL-visible SQL partitions is a separate DDL/data-movement
operation and must recheck the destination partition constraint.

### 2.9 Concurrency, garbage collection, and TRUNCATE

Readers do not block file production. They hold an immutable manifest handle;
retired files remain until no handle references their generation and the MVCC
horizon says no active snapshot can need them. Publication takes a short
partition-scoped manifest lock/CAS rather than a table-wide lock. Flush and
compaction for different partitions may run concurrently, while budget limits
prevent them from starving foreground queries.

TRUNCATE publishes an empty table generation transactionally. The existing
relfilenode-directory swap remains a valid implementation, but old directories
are reclaimed through the same generation-aware GC rules. DROP and failed DDL
likewise retire storage first and unlink it only when it is no longer visible.

The implementation sequence for this design lives in
[`ROADMAP.md`](../ROADMAP.md#parquet-table-engine).

## 3. System layers

```
                    ┌────────────────────────────────────────┐
   clients ────────▶│  pgwire (tokio): v3 protocol, TLS,     │
   (libpq/JDBC/...) │  SCRAM-SHA-256, cancel, COPY subproto  │
                    └───────────────┬────────────────────────┘
                    ┌───────────────▼────────────────────────┐
                    │  Session: GUC variables, prepared      │
                    │  statements, portals, cursors          │
                    └───────────────┬────────────────────────┘
                    ┌───────────────▼────────────────────────┐
                    │  Parser (sqlparser-rs, PG dialect)     │
                    └───────────────┬────────────────────────┘
                    ┌───────────────▼────────────────────────┐
                    │  Binder/Analyzer: name resolution,     │
                    │  type inference, view expansion,       │◀── Catalog
                    │  constraint/default resolution         │
                    └───────────────┬────────────────────────┘
                    ┌───────────────▼────────────────────────┐
                    │  Planner: cost-based (bottom-up, like  │
                    │  PG), join ordering, index selection,  │◀── Statistics
                    │  EXPLAIN-compatible output             │    (ANALYZE)
                    └───────────────┬────────────────────────┘
                    ┌───────────────▼────────────────────────┐
                    │  Executor (Volcano): SeqScan,          │
                    │  IndexScan, NestLoop/Hash/MergeJoin,   │
                    │  Agg, Sort, Limit, ModifyTable, ...    │
                    └──────┬──────────────────┬──────────────┘
                    ┌──────▼──────┐    ┌──────▼──────────────┐
                    │ Txn/MVCC:   │    │ storage-api:        │
                    │ XID, CLOG,  │    │ TableAm / IndexAm   │
                    │ snapshots,  │    │ (pluggable          │
                    │ SSI, locks  │    │  engines)           │
                    └──────┬──────┘    └──┬───────────┬──────┘
                           │      ┌───────▼─────┐ ┌───▼──────────────┐
                           │      │ pg-engine:  │ │ parquet-engine:  │
                           │      │ heap 8KB +  │ │ buffer tables,   │
                           │      │ RAM tables, │ │ manifests, sorted│
                           │      │ B-tree,TOAST│ │ immutable chunks │
                           │      └───────┬─────┘ └────────┬─────────┘
                    ┌──────▼──────────────▼───────────────▼──┐
                    │  WAL (core service, rmgr model):       │
                    │  append, group commit, fsync;          │
                    │  checkpointer; crash recovery (redo)   │
                    └────────────────────────────────────────┘
```

### 3.1 Catalog

- `pg_catalog` — real tables (pg_class, pg_attribute, pg_type, pg_proc,
  pg_namespace, pg_index, pg_constraint, …) with **the same OIDs for built-in
  objects** as PG (otherwise drivers that hardcode type OIDs break:
  `23 = int4`, `25 = text`, …).
- Bootstrap: the initial catalog is generated from upstream
  `pg_type.dat`/`pg_proc.dat` (build-time codegen).
- `information_schema` — views over pg_catalog, as in PG.
- An in-memory catalog cache with DDL-driven invalidation (sinval analog).

### 3.2 Type system

The most underestimated part of compatibility. Bit-for-bit behavior required:

- `numeric` — PG decimal arithmetic (own implementation, PG rounding behavior);
- `timestamptz`/`date`/`interval` — PG calendar math, `DateStyle`, timezone db;
- `text`/collations — ICU collations;
- arrays, composite types, ranges, `jsonb` (binary format + operators +
  jsonpath);
- text and binary encodings of every type for the protocol;
- implicit cast rules and function/operator overload resolution — we reproduce
  the semantics of `func_select_candidate` and PG's cast tables.

### 3.3 Functions and PL/pgSQL

- Built-in functions (~3000 in pg_proc): implemented by usage frequency; the
  rest return feature-not-supported with an honest SQLSTATE.
- PL/pgSQL — our own parser and interpreter (`crabgresql-plpgsql`) on top of
  our executor — a prerequisite for passing real-world migrations.

## 4. Workspace layout (crates)

```
crates/
  crabgresql-protocol        # pgwire: message codecs, auth, TLS
  crabgresql-parser          # sqlparser wrapper (PG dialect) + AST utilities
  crabgresql-catalog         # system catalogs, bootstrap, cache
  crabgresql-types           # type system: values, codecs, casts, numeric, datetime
  crabgresql-binder          # semantic analysis: AST -> logical plan
  crabgresql-planner         # optimizer: logical -> physical plan
  crabgresql-executor        # Volcano executor, expression eval
  crabgresql-txn             # XIDs, CLOG, snapshots, SSI, lock manager
  crabgresql-wal             # WAL append/replay, rmgr registry, checkpointer, recovery
  crabgresql-storage-api     # TableEngine/TableAm/IndexAm traits, Tid, TupleStream
  crabgresql-pg-engine       # default engine: 8KB heap, buffer pool, B-tree, TOAST
  crabgresql-parquet-engine  # buffered immutable Parquet chunks + compaction
  crabgresql-plpgsql         # PL/pgSQL parser + interpreter
  crabgresql-server          # session, GUCs, wiring it all together; bin: crabgresql
  crabgresql-pg-regress      # pg_regress-style runner; diff tests against PG
  crabgresql-bench           # analytical benchmark harness (ClickBench, TPC-H)
```

## 5. Compatibility verification strategy (this IS the product)

1. **Differential testing** — the primary tool. A runner executes the same SQL
   on real PostgreSQL (in a container) and on us, comparing results, column
   types, error messages and error codes. Corpora: PG regression tests
   (`src/test/regress`), sqllogictest, generative fuzzing (SQLsmith-style).
2. **Isolation**: we port PG's `src/test/isolation` (spec files with session
   interleavings) + **Jepsen/Elle** to validate SI/SSI under load.
3. **Driver matrix**: CI runs integration tests for libpq, JDBC, npgsql,
   psycopg, node-postgres, tokio-postgres, plus SQLAlchemy/Prisma/Hibernate
   smoke tests.
4. **Public compat dashboard**: % of PG regression tests passing, by category.

## 6. Deliberate deviations from PostgreSQL

Compatibility on the outside, modernity on the inside. Internally we allow
ourselves anything that is not visible through SQL:

- threads instead of processes, async I/O (io_uring on Linux);
- 64-bit XIDs (goodbye, wraparound) — while exposing `xmin`/`xmax` truncated to
  32 bits externally for compatibility;
- page checksums on by default, CRC on WAL records;
- no `postgresql.conf` legacy: we also read PG-format config; unknown GUCs are
  accepted and ignored with a warning (drivers love setting them).

## 7. Decisions made

| Question | Decision |
|---|---|
| Niche | General-purpose PostgreSQL replacement (OLTP) |
| Topology | Single-node until M5; distributed as a separate layer later |
| Parity version | **PostgreSQL 19**: grammar, catalogs, behavior, regression tests |
| Parser | sqlparser-rs (Apache), PG dialect; gaps closed via upstream PRs |
| Storage | `crabgresql-pg-engine` behind the pluggable `crabgresql-storage-api`; durable heap tables plus RAM-backed memory tables (`UNLOGGED`/`TEMP`) |
| Parquet layout | WAL-backed buffer tables, sorted immutable ~64 MiB chunks, manifest generations, and background compaction/repartitioning |
| Isolation | PG semantics ported 1:1 (RC/EvalPlanQual, RR=SI, SSI) |
| Executor | Volcano first, vectorization as opt-in later |
| Concurrency | tokio + threads, shared-everything |
| License | **Apache-2.0** — our own codebase; we do not port PostgreSQL directly |

### Clean-room approach

We reproduce PostgreSQL's *behavior*, not its code. Rules binding for the
entire codebase:

- It is **forbidden** to port PostgreSQL C code to Rust line-by-line or by
  "translation" — even where we match semantics 1:1 (visibility checks, the
  lock conflict matrix, cast rules, EvalPlanQual, SSI).
- It is **allowed** to rely on: the official PG documentation,
  architecture-level READMEs/comments (algorithm descriptions), publications
  (Lehman-Yao, ARIES, Cahill / Ports & Grittner on SSI), and the observable
  behavior of real PG via differential tests.
- Phrases in this document like "ported verbatim" mean *semantics*: the same
  decision logic, confirmed by tests — implemented independently.
- Borrowing upstream **data** is acceptable: catalog generation from
  `pg_type.dat`/`pg_proc.dat`, the regression- and isolation-test corpora. The
  PostgreSQL License (permissive, BSD-like) is compatible with Apache-2.0 —
  attribution goes into NOTICE.
- Error messages and EXPLAIN output match PG intentionally (they are part of
  compatibility) — short messages and output formats are not copyrightable in
  that sense, but we take them from observed behavior, not from the sources.
