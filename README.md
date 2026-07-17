# CrabgreSQL

A PostgreSQL-compatible DBMS written in Rust. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and roadmap.

**Status: M0 — "Hello, psql".** The pgwire v3 handshake and simple-query
protocol work end-to-end against the in-memory storage engine:

```console
$ cargo run -p crabgresql-server
$ psql -h 127.0.0.1 -p 5433
=> SELECT 1;
 ?column?
----------
        1
(1 row)

=> CREATE TABLE crabs (id integer, name text);
=> INSERT INTO crabs VALUES (1, 'ferris');
=> SELECT * FROM crabs;
```

What exists today:

- **Protocol** (`crabgresql-protocol`): startup phase (SSLRequest/GSSENC
  refused, CancelRequest accepted), trust auth, `ParameterStatus`,
  simple-query cycle with streamed result sets, `ErrorResponse` with real
  SQLSTATE codes. No TLS or SCRAM yet; extended-query messages fail cleanly
  (one error, skip until Sync, one ReadyForQuery — PG error recovery).
- **SQL** (`crabgresql-parser` + `crabgresql-server`): sqlparser-rs with the
  PG dialect; FROM-less `SELECT` over literals, single-table `SELECT`,
  `CREATE TABLE [IF NOT EXISTS]`, atomic `INSERT ... VALUES` (with PG-style
  untyped-literal coercion and NULL padding), no-op `SET`. int4/int8/text/bool
  only. Anything parsed but not executed (WHERE, ORDER BY, GROUP BY, LIMIT,
  JOIN, constraints, ...) errors with `0A000` instead of being silently
  ignored.
- **Execution** (`crabgresql-executor`): Volcano iterator nodes — `Values`,
  `SeqScan`, `Project`.
- **Storage** (`crabgresql-storage-api` + `crabgresql-memory-storage`): the
  pluggable `TableEngine`/`TableAm` API and its in-memory reference engine.

Tests: `cargo test` — unit tests per crate plus end-to-end tests that drive a
real driver (tokio-postgres) and raw-socket handshake checks.
