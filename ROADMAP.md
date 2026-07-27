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

The current implementation is V1: one or more immutable fragments per INSERT
statement, a 65,535-row `Tid` limit, transaction identity in each footer,
directory listing as the live-file index, `.pending` rename on commit,
sequential scans, and no background workers or partition support.

The target design should land in compatibility-preserving slices:

1. Add versioned layout and manifest metadata while continuing to read and
   write V1 fragments.
2. Introduce logical row IDs and the mandatory WAL-backed `BufferTable`,
   including recovery, snapshot reads, state transitions, memory limits, and
   backpressure.
3. Switch foreground file creation to sorted background flush with the 64 MiB
   target and V2 per-row engine metadata.
4. Add manifest-pinned scans, pruning/pushdown, retired-file GC, and leveled
   compaction.
5. Add internal partition split/merge and online repartitioning.
6. Only then consider UPDATE/DELETE through delta chunks and tombstones; the
   append-only path must be correct and recoverable first.
