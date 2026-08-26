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

v2 has begun. Columnar nodes live in `crabgresql-executor`'s `vector` module
**beside** the row nodes, never replacing them: a scan, an append, a filter, a
take-only projection and a sort. A columnar *segment* starts at a scan whose
engine offers `TableAm::scan_batches` and runs as far up as every operator has a
vectorized form; `Shred` turns batches back into tuples wherever it stops, so
the row executor above is unchanged and remains correct on its own.

Four rules keep this honest:

- **No knob.** The path is chosen automatically per node, when the engine can
  produce batches (`USING parquet`, `USING buffer`) and the expressions involved
  are provably equivalent. The heap engine is untouched, so the whole regression
  corpus keeps exercising the row path.
- **The planner decides, once.** `crabgresql_planner::vectorize` owns every
  eligibility rule, for the same reason `uses_hash_join` lives there: `EXPLAIN`
  and the executor must not drift. The executor decides only *how*.
- **Declining is free.** Anything outside the provable subset falls back to the
  row node silently. A fallback is never an error.
- **`EXPLAIN` says so.** A vectorized node renders a `(columnar: scan, filter,
  sort)` suffix — a deliberate divergence from PG's output, on the grounds that
  a plan which runs on a different engine than it appears to is worth less than
  the compatibility it costs. Row-path plans render exactly as before.

Where Arrow's semantics differ from PostgreSQL's, the type is excluded rather
than approximated: `numeric` (a decimal of the *column's* precision and scale,
which a constant carries no typmod to match), floats under equality (PG defines
`NaN = NaN` as true), `bpchar`
(blank-trimmed comparison), `timetz`/`interval` (structs with their own orders),
and text ordering under an ICU collation. Floats *are* usable as sort keys,
because canonicalizing `-0.0` and NaN makes Arrow's total order coincide with
PG's — ordering is repairable where equality is not; `numeric` sorts too, since
a column orders against itself and one decimal type covers it. `AND`/`OR` use Arrow's
Kleene kernels; the plain ones return NULL for `false AND NULL` and would
silently drop rows.

Arrow batches carry **`Value` semantics, not Arrow's** — a `Date32` holds
PostgreSQL epoch days. A format whose file layout is defined in Arrow's epoch
converts at its own boundary and nowhere else, so a relation's storage leaves
cannot disagree about what a date means.

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
  A checkpoint samples a redo point, flushes pages and the commit log, and records
  the redo point in `pg_control` (and in a `CHECKPOINT` record); recovery resumes
  there rather than at the head of the stream. Its fsync pass covers every relation
  *written* since the last checkpoint, not the ones it happened to find dirty: the
  buffer pool also writes pages back at eviction, and an evicted frame no longer
  says what it held, so the storage manager keeps the pending-fsync queue (as
  PostgreSQL's `md.c` does). A checkpoint declines to bound
  replay while any state's only durable trace is still a WAL record — rows in a RAM
  write buffer, or a committed TRUNCATE whose swap the catalog does not name yet —
  and records a whole-stream redo point instead.
  Implemented in `crabgresql-wal` (+ the heap engine's redo). A checkpoint is due
  once `CRABGRESQL_MAX_WAL_SIZE` bytes have been logged past the last one's sample,
  and the committing transaction runs it — that bound on replay is what a
  long-lived process has instead of a periodic checkpointer. Still deferred, all
  correct-but-unoptimized without them: a *time*-based checkpointer (an idle
  cluster's last commit stays unbounded until it shuts down), full-page writes for
  torn-page protection beyond page
  checksums, and WAL segment *recycling* — the log is cut into 32 MiB segment
  files (`pg_wal/<24 hex digits>`, PostgreSQL's naming) and a record never
  straddles a boundary. A checkpoint **removes** the segments lying wholly below
  the redo point it published (`CRABGRESQL_WAL_KEEP_SIZE` holds a tail of them
  back), so `pg_wal` settles at roughly `max_wal_size`. What is still deferred is
  reusing one under a future name: our segments are sparse and never
  preallocated, so a rename would save a single directory operation and cost
  three invariants — the insert position is derived from the highest segment
  present, replay checks a record's self-declared LSN only at the redo point, and
  the stream length is read off the last file. Recycling is worth doing together
  with preallocation, and not before.
- Syntactically, extensibility is exposed the standard PG way. Plain
  `CREATE TABLE` remains heap; `CREATE TABLE ... USING parquet ORDER BY (cols)`
  explicitly selects the columnar method and declares its layout order.
  `default_table_access_method` is unchanged.

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
collection. Clients select it with `CREATE TABLE ... USING parquet ORDER BY
(cols)`; its physical organization is not part of PostgreSQL's observable SQL
contract.

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
buffer limits        per-table soft bytes, a global hard limit, maximum row age
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
2. the clustering key follows;
3. the hidden logical row ID is the final tie-breaker.

The clustering key is **mandatory**, not optional: `CREATE TABLE ... USING
parquet` (or `USING buffer`) must declare it with `ORDER BY (columns)`, or carry
a `PRIMARY KEY` for it to default from, or the statement is refused with
`42P17`. A relation with no key would give up range pruning, compression
locality, and merge-friendly compaction all at once, and a store that silently
accepted one would hide all three. Nothing forces the choice either: every type
these methods store is B-tree-orderable, so a key can always be named. The row
ID's job is narrower than "supply an order when nothing else does" — it breaks
ties *within* equal keys, which is what makes the order total.

Comparisons use CrabgreSQL/PostgreSQL semantics, including direction, NULL
ordering, NaN behavior, and the recorded collation version. A collation-version
change makes affected chunks candidates for a rewrite rather than silently
changing their claimed order.

Two gaps between this section and today's implementation, both deliberate: the
recorded key carries no collation version yet, and it is stored as a field on the
relation schema rather than inside a versioned `ParquetLayout`. Relations created
before the key existed decode with an empty one, so the sorted flush must still
cope with an unordered relation even though DDL can no longer create one.

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

**What is implemented is a per-relation accounting and a single background
sweep, not a `BufferTableManager`.** The accounting is a resident-byte figure —
what the rows occupy in RAM, including `size_of::<Value>()` per column and each
value's own allocations — because a threshold measured in *encoded* bytes
under-reports a wide row several-fold and so admits several times the rows it
was set to admit.

Backpressure at the global limit is real: past it an autocommit write blocks
until a sweep brings the total back under. Two constraints shape where that
block can live, and both are load-bearing:

- It must happen **before the statement allocates its transaction ID**. The
  remedy for pressure is reclaiming rows no snapshot can still see, and the
  reclamation horizon is bounded by the oldest transaction in flight — so a
  waiter holding one is waiting for a flush its own wait prevents. For the same
  reason a write inside an explicit `BEGIN` block does not wait at all: the
  block's ID outlives the statement, so no point inside it is safe.
- Only relations a flush could actually relieve count toward the total. A
  standalone `USING buffer` relation has nowhere to flush to, so counting it
  would hold the limit shut permanently.

The bound is on the buffer, not on peak RSS: a flush additionally copies the
visible rows, encodes them into Arrow, and holds the originals until a snapshot
releases them, so draining a buffer costs a multiple of its size. Removing that
copy is a known follow-up.

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

The implemented form of that rule is coarser: no per-buffer LSN is tracked, so a
checkpoint records a whole-stream redo point whenever *any* buffer still holds
rows, and a bounded one only once every buffer is empty (`PgEngine::redo_floor`).
Per-buffer minimum-LSN tracking, which would let a cluster with resident buffered
rows still bound replay, is the refinement — and it is a change to that one
function's body.

That coarseness is also what makes an *unopenable* relation a startup refusal
rather than a clamp: a relation the engine could not open is not in its table map,
so it cannot be asked whether it holds rows, and a checkpoint would bound itself
as if it held none — retiring the segments that are those rows' only copy.
`PgEngine::refuse_if_unopenable_holds_rows` asks the replayed WAL directly instead
and refuses to come up, naming the directory to repair. Both that refusal and the
clamp are consequences of the same missing watermark, and both disappear with it.

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

### 3.0 Out-of-line attribute storage (TOAST)

The heap requires a tuple to fit one 8 KB page. An attribute too wide for that
is written to a **chunk relation** — a second relfilenode owned by the table —
and replaced in the tuple by a fixed-width pointer under a datum tag the value
codec never emits. `crabgresql-pg-engine/src/toast.rs` holds the format; the
seam is `tuple::decode_raw` (inline datums, safe under the page's frame lock)
followed by `RawTuple::resolve` (reassembly, deliberately after the lock drops).

What is stored out of line is exactly the bytes an inline datum would have been,
so detoasting is `concat(chunks)` then an ordinary decode — `text`, `bytea`,
`json`, `jsonb`, arrays and `tsvector` all take one path with no per-type logic.

Three choices worth knowing, all reproducing PostgreSQL's *behavior* rather than
its implementation:

- **Chunks are chained, not indexed.** PostgreSQL gives each value a `chunk_id`
  and indexes the chunk relation on `(chunk_id, chunk_seq)`. We put the first
  chunk's tid in the pointer and link each chunk to its successor through the
  on-page `ctid` field, which needs no sequence, no index and no index
  maintenance. The cost is that reads are strictly sequential — a slice read
  cannot skip ahead — and that the chunk relation cannot be vacuumed on its own.
  The pointer's `format` byte is the versioning seam if that trade stops paying.
- **A chunk is an ordinary on-page tuple**, with `natts = 0` and the `ctid` field
  repurposed as the chain link. So chunk writes reuse the heap's placement path
  and log `HEAP_INSERT`, reclamation logs `HEAP_VACUUM`, and crash recovery needs
  no new code: the heap's redo handler applies those records to a page without
  caring which relfilenode it belongs to. No new resource manager exists.
- **Reclamation is driven from the heap side.** Nothing is freed eagerly —
  because a chain must stay readable for as long as any snapshot can reach the
  tuple naming it. `VACUUM` frees the heap slots first and the chunks second; a
  crash in that gap leaks chunks nothing references, where the other order would
  free chunks a later write had reused. Its victim rule covers both ways to be
  dead: a version a committed transaction deleted below the horizon, and one
  whose *inserter* aborted below it — the latter matters far more with
  out-of-line storage, since a rolled-back wide INSERT strands a whole value
  rather than at most one page-sized tuple.

Ordering that makes it crash-safe: the chunk relfilenode reaches the durable
catalog before any chunk reaches the log (or the startup orphan sweep would
unlink the file), and the chunks are logged before the tuple pointing at them
(the WAL's total order then guarantees a pointer can never become durable ahead
of its target — no extra fsync involved). Two consequences follow from the first
rule and are load-bearing. Creation is serialized, because two writers each
publishing a store would leave one of them unnamed by the catalog and therefore
swept. And a chunk store keeps its relfilenode for the table's whole life:
TRUNCATE *empties* the file rather than swapping it as it does the heap, since a
second relfilenode would be named by no WAL record and would reach the catalog
only at commit — a crash in that window would unlink a file a committed row
points into. An UPDATE writes its chains only after winning the right to replace
the row, since chunks written by the loser of that race are named by no tuple and
so are reachable by no reclamation path.

A row is toasted widest-attribute-first until it fits. The width floor below
which externalizing is not worth it is a preference, not a limit: if honouring it
would leave the row unstorable, the planner drops to just above the pointer width
and continues, so a row of many medium-width attributes is stored rather than
refused (PostgreSQL has no such floor). A single value is capped at 1 GB, as in
PostgreSQL.

Not implemented: **compression**. PostgreSQL compresses before externalizing, so
a few-KB value stays inline there and goes out of line here. Also not
implemented: per-attribute `attstorage` / `ALTER TABLE ... SET STORAGE`. And the
chunk relation is created lazily, on the first row that needs it, where
PostgreSQL creates one at `CREATE TABLE` for any table with a variable-length
column — so `pg_class.reltoastrelid` stays 0 for longer than PostgreSQL's does
(0 is itself legitimate PostgreSQL state, reported for a table with no TOAST
relation).

A row that still does not fit once everything eligible has been moved out —
one whose columns are all fixed-width — raises `54000 program_limit_exceeded`,
`row is too big: size N, maximum size 8160`.

### 3.1 Catalog

- `pg_catalog` — real tables (pg_class, pg_attribute, pg_type, pg_proc,
  pg_namespace, pg_index, pg_constraint, …) with **the same OIDs for built-in
  objects** as PG (otherwise drivers that hardcode type OIDs break:
  `23 = int4`, `25 = text`, …).
- Bootstrap: the initial catalog is generated from upstream
  `pg_type.dat`/`pg_proc.dat`/`pg_cast.dat` by `crabgresql-bki`, a build-time
  codegen library. It runs in two phases — every catalog first declares the
  symbols it defines, and only then does emission resolve references — because
  the reference graph is cyclic: `pg_type.typinput` names a `pg_proc` row whose
  `prorettype` names a `pg_type` row, and no ordering of the files makes a
  single pass work. The `regproc` references — `pg_type.typinput`,
  `pg_cast.castfunc`, `pg_am.amhandler` — resolve to real OIDs, and `pg_proc`
  gets rows for **exactly the functions those references name**. The rest of
  `pg_proc.dat` is deliberately left out: a `pg_proc` row is a claim that a
  function exists, and this build runs its SQL surface from its own registry
  rather than upstream's list. `every_regproc_reference_resolves_to_an_emitted_row`
  fails if a reference ever dangles.
- Known `pg_catalog` gaps, in the order upstream's `type_sanity` trips over
  them: `typrelid` is 0 for the catalog composite types, there is no
  `pg_range`/`pg_opclass`, there are no domain or range types, and
  `pg_attribute` carries neither system columns nor `attislocal`/`attinhcount`
  (per-column inheritance provenance is not recorded — the parent↔child
  correspondence is recomputed by name).
- `information_schema` — views over pg_catalog, as in PG. Its seven `_pg_*`
  type-shape helpers (`_pg_char_max_length`, `_pg_char_octet_length`, the three
  `_pg_numeric_*`, `_pg_datetime_precision`, `_pg_interval_type`) are callable
  functions, and the `columns`/`domains` row builders answer through the same
  `crabgresql_types::info_schema` module rather than a second copy of the rules
  — PostgreSQL *defines* those views in terms of those functions, so a view
  column and a direct call cannot drift apart. No `pg_proc` row is published for
  them, so `\df` does not list them and `::regprocedure` will not resolve one.
- Which relations are served is one table: `registry::CATALOG_RELATIONS` in
  `crabgresql-catalog`, pairing each name and OID with the two `fn` pointers
  that build it (`fn() -> TableSchema` and `fn(&SystemCatalog) -> rows`). The
  served set and the OID table are therefore the same set by construction, so
  `'pg_class'::regclass` cannot resolve for a relation nothing serves. Adding a
  catalog is a module under `src/catalogs/` plus one registry line; the single
  `rows` signature means a relation with unusual inputs adds no argument to any
  shared call site.
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

### 3.4 The data directory

A cluster is one directory, laid out as PostgreSQL lays one out wherever the
contents serve the same purpose:

```
<PGDATA>/
  PG_VERSION           # major version that wrote this directory
  postmaster.pid       # the running server's PID; absent when none is running
  crabgresql_authid    # the cluster's roles (pg_authid), owner-only
  base/                # one file per heap/index relfilenode
  global/
    pg_control         # redo point, XID floor, clean-shutdown flag
    relcatalog         # the engine's relation catalog (name -> filenode+schema)
  pg_wal/              # 32 MiB segments, named by segment number
  pg_xact/             # commit log
  stats/               # one file of ANALYZE results per relation
  parquet/             # one directory per Parquet relation
```

Creation is a single operation (`crabgresql_server::initdb`), reached either
from `crabgresql initdb -D <dir>` or from the server itself when it is started
on an absent or empty directory. The two differ in one answer: `initdb` refuses
a directory that already holds a cluster, and the server expects one.

Roles are a cluster object, so the role catalog is created there too — before
`PG_VERSION`, like everything the stamp vouches for. `--superuser` names the
bootstrap role and `--pwfile` gives it a password, hashed into a SCRAM verifier
on the way to disk. Only `initdb` takes a password: the server creating a
cluster in passing has nowhere to have got one from, so a role it bootstraps
has none and is therefore trusted (§3.5).

`PG_VERSION` is written **last** and fsynced, which is what makes it a marker
rather than a field: everything it vouches for is on disk before it is. It
states the major version whose behavior the writing server reported, not the
layout of any one file — those carry their own version fields (`pg_control`'s
header, the relation catalog's per-section magics), which is where a format
change *within* a major version is caught.

A directory holding a control file or `base/` but no `PG_VERSION` is a cluster
written before the stamp existed. It is adopted — stamped in place, data
untouched — because the on-disk format is a compatibility boundary. A directory
holding anything else is refused: it was almost certainly named by mistake.

Opening a cluster is exclusive (`crabgresql_server::lockfile`). `PG_VERSION`
says whose directory this is and of what version; `postmaster.pid` says whether
somebody has it open *now*. It is created with `O_EXCL` **before the cluster is
created**, let alone opened — creating one is as much a write as any other, so
the order is: create the directory, take the lock, then initialize. That is why
a directory holding nothing but a lock file still counts as empty. The file
carries the eight lines PostgreSQL writes (PID, directory, start time, port,
socket directory, listen address, shared-memory key, status) and is unlinked
when the server exits.

An existing file whose PID is alive (`kill(pid, 0)`) stops the start. One whose
PID is dead is a stale file a crash left, and is taken over. Anything else —
empty, unreadable, not ours — is refused and left alone: an empty file is
indistinguishable from a server between its own `O_EXCL` create and its first
write, so unlinking it is how two servers end up in one cluster. Two servers
replaying one WAL into one set of relation files is the failure all of this
rules out, and the `initdb` subcommand takes the same lock, since rewriting the
skeleton of a live cluster is that failure with a different command line.

### 3.5 Authentication

SCRAM-SHA-256 (`crabgresql-server/src/auth.rs`), against the verifier stored in
the role catalog. The exchange is a pure state machine — the connection handler
does the reading and writing, the crypto lives with the verifier format in
`roles::scram`, and this decides only what the answers are.

There is no `pg_hba.conf`, so *whether* a connection must authenticate is read
off the catalog instead:

* a role with a stored password must pass SCRAM;
* a role without one is trusted, which is how every cluster starts and how one
  that never sets a password keeps behaving;
* a name the catalog does not have gets a synthetic superuser session — but
  only while nothing in the cluster has a password. Once something does, the
  fallback would hand a superuser session to anyone connecting under an unused
  name, so it becomes a FATAL `28000` instead.

`SCRAM-SHA-256-PLUS` is never advertised: channel binding binds the exchange to
a TLS session and there is no TLS. An `md5…` password — one a client supplied
pre-hashed — is refused with a message saying so, rather than answered with a
method this server does not implement.

## 4. Workspace layout (crates)

```
crates/
  crabgresql-protocol        # pgwire: message codecs, auth, TLS
  crabgresql-parser          # sqlparser wrapper (PG dialect) + AST utilities
  crabgresql-catalog         # system catalogs, bootstrap, cache
  crabgresql-bki             # build-time codegen of pg_catalog from vendored .dat
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
  crabgresql-server-process  # start that binary as a child process, for the harnesses
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

One deviation *is* visible through SQL, and is listed here because it is the
exception to the paragraph above:

- **`CREATE TABLE ... ORDER BY (columns)`** declares the layout sort key of an
  engine-managed relation (§2.2). PostgreSQL has no such clause — the rule is
  ClickHouse MergeTree's — and for `USING parquet`/`USING buffer` it is
  mandatory, so a statement PostgreSQL accepts can be refused here with `42P17`.
  It changes nothing for a heap table, which rejects the clause outright, so the
  PostgreSQL-compatible surface is untouched: the divergence is reachable only
  from a `USING` clause that already names a non-PostgreSQL access method.

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
