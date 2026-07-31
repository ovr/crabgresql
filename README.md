# CrabgreSQL

A PostgreSQL-compatible DBMS written in Rust. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and
[ROADMAP.md](ROADMAP.md) for the development sequence.

**Status: M1 in progress — CRUD.** The full parse → bind → plan → execute
pipeline runs simple CRUD end-to-end against the in-memory storage engine:

```console
$ cargo run -p crabgresql-server
$ psql -h 127.0.0.1 -p 5433
=> CREATE TABLE crabs (id integer, name text);
=> INSERT INTO crabs VALUES (1, 'ferris'), (2, 'hermit');
=> SELECT id + 1, name FROM crabs WHERE id <> 2;
=> UPDATE crabs SET name = 'red' WHERE id = 1;
=> DELETE FROM crabs WHERE name = 'red';
```

What exists today:

- **Protocol** (`crabgresql-protocol`): startup phase (SSLRequest/GSSENC
  refused, CancelRequest accepted), trust auth, `ParameterStatus`,
  simple-query cycle with streamed result sets, `ErrorResponse` with real
  SQLSTATE codes. No TLS or SCRAM yet; extended-query messages fail cleanly
  (one error, skip until Sync, one ReadyForQuery — PG error recovery).
- **SQL** (`crabgresql-parser` → `crabgresql-binder` → `crabgresql-planner`):
  sqlparser-rs with the PG dialect; the binder resolves names and infers
  types with PG semantics (int4/int8 promotion, untyped-literal coercion,
  `unknown` handling, PG error messages and SQLSTATEs), the planner maps the
  logical plan onto scan pipelines. Supported: FROM-less and table-backed
  `SELECT` with expressions, filtering, ordering, limits, grouping/aggregates,
  comma/`CROSS JOIN`, and `INNER`/`LEFT`/`RIGHT`/`FULL JOIN ... ON`;
  `CREATE TABLE [IF NOT EXISTS]` with `NOT NULL`, per-row `DEFAULT`, and
  `PRIMARY KEY`/`UNIQUE` constraints, semantic `CREATE [UNIQUE] INDEX`,
  atomic `INSERT ... VALUES`, `UPDATE ... SET ... [WHERE]`,
  `DELETE FROM ... [WHERE]`, and no-op `SET`. Anything parsed but not executed
  (including `JOIN USING`/`NATURAL JOIN`, foreign keys, checks, and advanced
  index forms) errors with `0A000`
  instead of being silently ignored.
- **Expressions** (`crabgresql-executor::eval`): comparisons, `AND`/`OR`/`NOT`
  with SQL three-valued NULL logic, `IS [NOT] NULL`, int4/int8 arithmetic
  with PG overflow (`22003`) and division-by-zero (`22012`) behavior.
- **Execution** (`crabgresql-executor`): Volcano iterator nodes — `Values`,
  `SeqScan`, `Filter`, `Projection`, nested-loop joins, aggregation, sorting,
  and limits — plus buffered (statement-atomic) INSERT/UPDATE/DELETE with
  NOT NULL and immediate composite-uniqueness validation.
- **Storage** (`crabgresql-storage-api`): the pluggable `TableEngine`/`TableAm`
  API over a durable 8 KB heap (`crabgresql-pg-engine`), RAM-backed temporary
  and WAL-skipped unlogged tables, plus managed permanent append-only Snappy
  Parquet tables (`crabgresql-parquet-engine`) selected with
  `CREATE TABLE ... USING parquet ORDER BY (cols)` — append-only per row, with a
  transactional TRUNCATE that swaps in a fresh fragment directory. The layout
  order is mandatory (a `PRIMARY KEY` supplies it too), following ClickHouse
  MergeTree rather than PostgreSQL, which has no such clause.

Tests: `cargo test` — unit tests per crate plus end-to-end tests that drive a
real driver (tokio-postgres) and raw-socket handshake checks.

## Configuration

Every environment variable the server reads is declared in one place,
[`crabgresql-config`](crates/crabgresql-config/src/lib.rs); a value that does
not parse falls back to its default rather than failing startup.

| Variable | Default | Range | Controls |
| --- | --- | --- | --- |
| `CRABGRESQL_PORT` | `5433` | | TCP port to listen on (also `--port`) |
| `PGDATA` | `./pgdata` | | data directory the durable heap engine is opened in (also `--data-dir`) |
| `RUST_LOG` | `info` | | tracing filter directives |
| `CRABGRESQL_BUFFER_TABLE_SOFT_BYTES` | `32MB` | `1MB`–`2GB` | per-relation buffered bytes that make one write buffer flush-eligible |
| `CRABGRESQL_BUFFER_GLOBAL_HARD_BYTES` | `256MB` | `1MB`–`16GB` | buffered bytes across all relations that make every buffer eligible |
| `CRABGRESQL_BUFFER_MAX_AGE` | `1m` | `10ms`–`24h` | how long a write buffer may hold rows before being flushed anyway |
| `CRABGRESQL_BUFFER_TICK` | `1s` | `10ms`–`1h` | how often the background flush worker looks for eligible buffers |

Sizes take a bare byte count or a binary unit — `kB`, `MB`, `GB`, `TB`, with
the trailing `B` optional, so `33554432`, `32MB` and `32m` all say the same
thing. Durations take a bare count of milliseconds or a unit — `ms`, `s`, `m`
(also `min`), `h` — so `60000`, `60s` and `1m` are one value. Units are matched
case-insensitively; mind that in a duration `m` is minutes and `ms` is
milliseconds.

A value outside the supported range is clamped to the nearest end of it, and
one that cannot be read at all falls back to the default; either way the server
logs a warning and starts, because a typo in a tuning knob should not keep it
down.

The two `*_BYTES` knobs count what buffered rows occupy **in RAM**, not what
they would serialize to. A row costs `size_of::<Value>()` per column whatever
that column holds, so a wide analytics table runs several kilobytes a row and a
32 MB buffer holds proportionally fewer rows than its encoded size suggests.
Past `CRABGRESQL_BUFFER_GLOBAL_HARD_BYTES` an autocommit write waits for the
flush worker to make room rather than adding to the total; a write inside an
explicit transaction block does not, because it already holds the transaction
ID that bounds what a flush is allowed to reclaim.

The `CRABGRESQL_BUFFER_*` knobs are environment variables rather than GUCs
because a `SET` is session-scoped and the flush worker is process-wide; moving
them to real storage settings is a follow-up.

## PostgreSQL regression tests

The PostgreSQL regression corpus (`src/test/regress`, pinned to a master
commit) is vendored under [`vendor/postgres/`](vendor/postgres/README.md) —
populate or bump it with `scripts/sync-regress.sh`. The pg_regress-style
runner in `crabgresql-pg-regress` executes the scripts against an in-process
server, emulating `psql -a -q` output, and diffs against `expected/*.out`:

```console
$ cargo run -p crabgresql-pg-regress --bin regress            # full schedule (compat %)
$ cargo run -p crabgresql-pg-regress --bin regress -- --tests boolean,int4
11 of 245 tests passed (4%).
See target/regress/regression.diffs for details.
```

The score is the compatibility dashboard, so a near-zero percentage at M0 is
expected and honest. Regression protection lives in `cargo test`: the
crabgresql-authored smoke suite must always pass, plus every upstream test
promoted to `crates/crabgresql-pg-regress/suites/upstream_must_pass.txt` as
coverage grows.

## Benchmarks

`crabgresql-bench` runs published analytical benchmarks — ClickBench for scans
and aggregation over one wide table, TPC-H for joins over eight — against an
in-process server, or against stock PostgreSQL for comparison. A query that hits
an engine gap is reported in place instead of aborting the run, so the results
table doubles as a gap list. See
[`crates/crabgresql-bench/README.md`](crates/crabgresql-bench/README.md).

```console
$ cargo run --release -p crabgresql-bench -- run clickbench --data hits.tsv
43 of 43 queries succeeded, 13.228s total (best runs)

$ cargo run --release -p crabgresql-bench -- run tpch --data tpch/ --timeout 180
22 of 22 queries succeeded, 123.329s total (best runs)  # not a TPC-H result
```
