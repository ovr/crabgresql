# CrabgreSQL

A PostgreSQL-compatible DBMS written in Rust. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and roadmap.

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
- **Storage** (`crabgresql-storage-api` + `crabgresql-pg-engine`): the pluggable
  `TableEngine`/`TableAm` API — tid-addressed scan/insert/update/delete — over a
  durable 8 KB slotted-page heap with a buffer pool and WAL, plus RAM-backed
  memory tables (`UNLOGGED`/`TEMP`) that reuse the same heap AM without a WAL.

Tests: `cargo test` — unit tests per crate plus end-to-end tests that drive a
real driver (tokio-postgres) and raw-socket handshake checks.

## PostgreSQL regression tests

The PostgreSQL regression corpus (`src/test/regress`, pinned to a master
commit) is vendored under [`vendor/postgres/`](vendor/postgres/README.md) —
populate or bump it with `scripts/sync-regress.sh`. The pg_regress-style
runner in `crabgresql-pg-regress` executes the scripts against an in-process
server, emulating `psql -a -q` output, and diffs against `expected/*.out`:

```console
$ cargo run -p crabgresql-pg-regress --bin regress            # full schedule (compat %)
$ cargo run -p crabgresql-pg-regress --bin regress -- --tests boolean,int4
9 of 245 tests passed (3%).
See target/regress/regression.diffs for details.
```

The score is the compatibility dashboard, so a near-zero percentage at M0 is
expected and honest. Regression protection lives in `cargo test`: the
crabgresql-authored smoke suite must always pass, plus every upstream test
promoted to `crates/crabgresql-pg-regress/suites/upstream_must_pass.txt` as
coverage grows.
