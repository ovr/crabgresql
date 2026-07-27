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

**`crabgresql-parquet-engine`** — managed append-only analytics storage:

- one or more immutable Snappy-compressed fragments per INSERT statement,
  capped at 65,535 rows so `(fragment, row)` maps to a stable `Tid`;
- only user columns appear in the Arrow/Parquet schema; format, schema, `xmin`,
  and `cmin` metadata live in the footer;
- fragments are fsynced as `.pending`, covered by an XID-observation WAL record,
  and atomically promoted on commit or removed on abort/recovery;
- sequential row-group scans feed the existing row executor. Physical index
  scans and predicate/projection pushdown are intentionally deferred;
- TRUNCATE is transactional, by the same mechanism as the heap's relfilenode
  swap: a fresh `parquet/<new>/` fragment directory is staged under an
  `AccessExclusive` hold and WAL-logged, then swapped in on commit (old directory
  removed) or discarded on abort — so a rollback or a crash before commit
  restores every row, and a crash after it is repaired from the WAL;
- V1 rejects UPDATE and DELETE (fragments are immutable), TEMP/UNLOGGED tables,
  partitioning, external `LOCATION`, and types without a native or documented
  compound encoding.

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

## 2. System layers

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
                           │      │ pg-engine:  │ │ memory tables:   │
                           │      │ heap 8KB,   │ │ Parquet + memory │
                           │      │ buffer pool,│ │ table methods    │
                           │      │ B-tree,TOAST│ │ managed storage  │
                           │      └───────┬─────┘ └────────┬─────────┘
                    ┌──────▼──────────────▼───────────────▼──┐
                    │  WAL (core service, rmgr model):       │
                    │  append, group commit, fsync;          │
                    │  checkpointer; crash recovery (redo)   │
                    └────────────────────────────────────────┘
```

### 2.1 Catalog

- `pg_catalog` — real tables (pg_class, pg_attribute, pg_type, pg_proc,
  pg_namespace, pg_index, pg_constraint, …) with **the same OIDs for built-in
  objects** as PG (otherwise drivers that hardcode type OIDs break:
  `23 = int4`, `25 = text`, …).
- Bootstrap: the initial catalog is generated from upstream
  `pg_type.dat`/`pg_proc.dat` (build-time codegen).
- `information_schema` — views over pg_catalog, as in PG.
- An in-memory catalog cache with DDL-driven invalidation (sinval analog).

### 2.2 Type system

The most underestimated part of compatibility. Bit-for-bit behavior required:

- `numeric` — PG decimal arithmetic (own implementation, PG rounding behavior);
- `timestamptz`/`date`/`interval` — PG calendar math, `DateStyle`, timezone db;
- `text`/collations — ICU collations;
- arrays, composite types, ranges, `jsonb` (binary format + operators +
  jsonpath);
- text and binary encodings of every type for the protocol;
- implicit cast rules and function/operator overload resolution — we reproduce
  the semantics of `func_select_candidate` and PG's cast tables.

### 2.3 Functions and PL/pgSQL

- Built-in functions (~3000 in pg_proc): implemented by usage frequency; the
  rest return feature-not-supported with an honest SQLSTATE.
- PL/pgSQL — our own parser and interpreter (`crabgresql-plpgsql`) on top of
  our executor — a prerequisite for passing real-world migrations.

## 3. Workspace layout (crates)

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
  crabgresql-parquet-engine  # managed append-only Parquet table access method
  crabgresql-plpgsql         # PL/pgSQL parser + interpreter
  crabgresql-server          # session, GUCs, wiring it all together; bin: crabgresql
  crabgresql-pg-regress      # pg_regress-style runner; diff tests against PG
```

## 4. Compatibility verification strategy (this IS the product)

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

## 5. Deliberate deviations from PostgreSQL

Compatibility on the outside, modernity on the inside. Internally we allow
ourselves anything that is not visible through SQL:

- threads instead of processes, async I/O (io_uring on Linux);
- 64-bit XIDs (goodbye, wraparound) — while exposing `xmin`/`xmax` truncated to
  32 bits externally for compatibility;
- page checksums on by default, CRC on WAL records;
- no `postgresql.conf` legacy: we also read PG-format config; unknown GUCs are
  accepted and ignored with a warning (drivers love setting them).

## 6. Roadmap

- **M0 — Hello, psql** (protocol): pgwire, auth, `SELECT 1`, simple query
  execution on an in-memory table, error messages. Check: psql works,
  pgbench -i fails meaningfully.
- **M1 — CRUD + catalog**: storage-api + the heap engine in full,
  CREATE TABLE/INSERT/SELECT/UPDATE/DELETE, pg_catalog, the
  int/text/bool/numeric/timestamptz types, `\d` works in psql. `pg-engine`
  development starts in parallel.
- **M2 — Transactions**: MVCC, snapshots, READ COMMITTED + REPEATABLE READ,
  row locks, WAL + recovery, B-tree, ORM smoke tests pass.
- **M3 — SERIALIZABLE**: SSI, deadlock detection, PG isolation tests green,
  Elle finds no anomalies.
- **M4 — Server-side logic**: PL/pgSQL, triggers, views, COPY, cursors,
  LISTEN/NOTIFY; Flyway/Prisma migrations of real projects pass.
- **M5 — Ops**: pg_stat_* views, EXPLAIN ANALYZE (partial: the statement runs and
  is timed, with `Planning Time:` / `Execution Time:` footers; per-node
  `(actual …)` counters still to come. `VERBOSE`, `FORMAT JSON`/`XML`/`YAML`,
  `SETTINGS`, `MEMORY`, `WAL` and `GENERIC_PLAN` now report `0A000` rather than
  being silently ignored — they would change the shape of the output, and a plan
  that answers a question the client did not ask is worse than a stated gap),
  automatic VACUUM/ANALYZE, logical replication (publisher) for CDC.

## 7. Decisions made

| Question | Decision |
|---|---|
| Niche | General-purpose PostgreSQL replacement (OLTP) |
| Topology | Single-node until M5; distributed as a separate layer later |
| Parity version | **PostgreSQL 19**: grammar, catalogs, behavior, regression tests |
| Parser | sqlparser-rs (Apache), PG dialect; gaps closed via upstream PRs |
| Storage | `crabgresql-pg-engine` behind the pluggable `crabgresql-storage-api`; durable heap tables plus RAM-backed memory tables (`UNLOGGED`/`TEMP`) |
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
