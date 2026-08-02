# CrabgreSQL Roadmap

The architecture and its long-lived invariants are documented in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). This file tracks implementation
order and may change as milestones are completed or reprioritized.

## Project milestones

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

## Parquet table engine

The target design is specified in
[`docs/ARCHITECTURE.md` §2](docs/ARCHITECTURE.md#2-parquet-table-engine).

The current implementation is V1 plus the write buffer: fragments carry
transaction identity in their footer, the directory listing is the live-file
index, `.pending` is renamed on commit, and scans are sequential. Foreground
`INSERT`/`COPY` no longer create files — they land in a WAL-logged RAM
`BufferTable` (the `buffer` access method, also selectable on its own with
`CREATE TABLE ... USING buffer`), which a background worker or an explicit
`VACUUM` flushes into one fragment. A relation therefore plans as an `Append`
over its two engine-internal storage leaves.

Two consequences worth stating plainly:

- **A committed `INSERT` is durable before any file exists.** It is covered by
  the commit record's fsync and rebuilt from the WAL at startup, but nothing
  external should expect to see it under `parquet/<rel>/` until a flush.
- **WAL volume now carries all Parquet data.** With no segment recycling and
  whole-WAL replay at every boot, a large `COPY` grows both the log and every
  subsequent startup. Checkpoint-bounded recovery is therefore a hard
  prerequisite of step 3 below, not a deferred nicety.

The target design should land in compatibility-preserving slices:

1. Add versioned layout and manifest metadata while continuing to read and
   write V1 fragments.
2. ~~Introduce logical row IDs and the mandatory WAL-backed `BufferTable`,
   including recovery, snapshot reads, state transitions, memory limits, and
   backpressure.~~ **Done**, except per-partition buffers and write
   backpressure: there is one buffer per relation, and memory is bounded by the
   flush policy rather than by blocking writers.
3. Switch foreground file creation to sorted background flush with the 64 MiB
   target and V2 per-row engine metadata. The flush is background already, and
   each write is now sorted on the relation's `ORDER BY` key: a write is sorted
   whole before it is cut into fragments, so the fragments *of that write* have
   disjoint key ranges, and each says so in its row-group `sorting_columns`.
   Two things remain. **Size**: a fragment is still capped at 65,535 rows by the
   `Tid` offset, so the 64 MiB target is unreachable until the V2 footer lands.
   **Clustering across writes**: every write is its own sorted run and the runs
   overlap freely, so a relation loaded by many flushes is still unclustered as
   a whole — pruning can exclude fragments only within a run until step 4's
   compaction merges the runs.
4. Add manifest-pinned scans, pruning/pushdown, retired-file GC, and leveled
   compaction.
5. Add internal partition split/merge and online repartitioning.
6. Only then consider UPDATE/DELETE through delta chunks and tombstones; the
   append-only path must be correct and recoverable first.
