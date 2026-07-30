# crabgresql-bench

A harness for running published analytical benchmarks against CrabgreSQL. It
boots a server in-process (or connects to an external one), loads the dataset
once, times every query, and prints a results table — or JSON shaped like the
upstream benchmarks' own result files.

Failing queries do not abort the run: a query that hits a missing function or
an unsupported construct is reported in place, so the results table doubles as
a gap list, and `--json` carries the reasons alongside the timings. A lost
connection reconnects and is reported as such, so one dead connection cannot
masquerade as a screenful of engine gaps.

What the harness does **not** check is whether a result is *correct* — a query
is scored on not erroring. It does flag a query whose row count moves between
runs, but the real cross-check is running the same suite against stock
PostgreSQL with `--url` and comparing.

## Suites

| Suite | Tables | Queries | Dataset |
| --- | --- | --- | --- |
| `clickbench` | `hits` (105 columns) | 43 | [hits.tsv.gz](https://datasets.clickhouse.com/hits_compatible/hits.tsv.gz), 100M rows / ~70 GB uncompressed |
| `tpch` | 8, `region` … `lineitem` | 22 | generated locally, any scale factor |

`bench list` prints the same thing.

The two are complements: ClickBench is one wide denormalized table, so it
measures scans, filters and aggregation; TPC-H is eight tables and 22 queries
built out of joins, subqueries and correlation, so it measures the planner.

## Running ClickBench

Get the data. The full set is 100M rows; take a slice while the engine is
young — gzip is a stream, so a truncated download decompresses fine up to the
cut:

```console
$ curl -sS -r 0-120000000 https://datasets.clickhouse.com/hits_compatible/hits.tsv.gz \
    | gzip -dc 2>/dev/null | head -n 1000000 > hits.tsv
```

Then run it:

```console
$ cargo run --release -p crabgresql-bench -- run clickbench --data hits.tsv --runs 1
...
 34     0.293     0.293        10  ok
 35     0.302     0.302        10  ok
 36     0.296     0.296        10  ok

43 of 43 queries succeeded, 13.228s total (best runs)
```

Both sample outputs above are `--runs 1`; the default is 3, which adds a column
per repetition and reports the best of them.

Build in `--release`; a debug build measures the compiler, not the engine.

## Running TPC-H

There is nothing to download: TPC-H data is generated. DuckDB's `tpch`
extension is the least ceremony:

```console
$ duckdb tpch.duckdb -c "INSTALL tpch; LOAD tpch; CALL dbgen(sf=0.01);"
$ mkdir -p tpch
$ for t in region nation part supplier partsupp customer orders lineitem; do
    duckdb tpch.duckdb -c "COPY $t TO 'tpch/$t.tbl' (FORMAT csv, DELIMITER '|', HEADER false);"
  done
```

TPC's own `dbgen` works too — it terminates every line with a trailing `|`,
which `COPY` would read as one extra empty column, so the loader strips it.

`--data` is then the *directory*, not a file. The suite has eight tables and
each is loaded from `<dir>/<table>.tbl`; only that extension is accepted,
because the `COPY` options come from the suite's format, so a file named for a
different one would be read with the wrong delimiter:

```console
$ cargo run --release -p crabgresql-bench -- run tpch --data tpch/ --runs 1
...
 19    82.152    82.152         1  ok
 20    11.045    11.045         1  ok

22 of 22 queries succeeded, 123.329s total (best runs)
```

All 22 run at `sf=0.01`, and each returns the same row count as stock
PostgreSQL 18 on the same data. Three are slow enough to need a raised
`--timeout`: Q19 (~82s), Q4 and Q20/Q21 (~10-18s). Q19 is the extreme because
its whole `WHERE` is one top-level `OR`, so there is no conjunct for the
pushdown pass to extract into a join condition and it degenerates into a cross
product; the rest are correlated subqueries, re-planned once per outer row.

The queries are the specification's own text — implicit comma joins, no
hand-rewriting into `JOIN … ON`. Rewriting them would measure the rewrite
rather than the planner, and a query the planner handles badly is exactly what
this harness exists to surface.

Start small. TPC-H is quadratic in the wrong places for a young planner, so
`sf=0.01` is a sensible first run and `sf=1` is not.

## How the load works

The load streams in as a sequence of `COPY` batches, because the server
materializes a whole `COPY` payload before inserting any of it — one statement
for the full file would OOM the (in-process) server long before the engine's
own limits mattered. Even so, the loaded data has to fit in the machine, so
take a slice with `--rows` until the engine grows out-of-core execution.

## Options that matter

- `--rows N` — load only the first N rows. Single-table suites only: slicing
  each table of a joined schema at the same count leaves the keys dangling, so
  the joins would match almost nothing while every query still reported `ok`.
  Shrink a multi-table suite by regenerating at a smaller scale factor.
- `--data-dir DIR` — put the server's data directory somewhere persistent.
  A later run over the same directory sees the tables already loaded and skips
  the load entirely; `--reload` forces a rebuild. A reused dataset's storage is
  reported as `unknown`, and `--using`/`--rows` are refused rather than
  silently ignored, since neither can be honored without rebuilding. A dataset
  that is only partly there is refused too, rather than half-loaded — the check
  is on rows, not on the tables merely existing, because they are all created
  before any of them is filled.
- `--using AM` — create the tables with an access method (`heap`, `parquet`,
  `buffer`) instead of leaving it to the server's default. Every report names the
  storage its numbers were measured on. `parquet` and `buffer` require a layout
  sort key, which is spliced from the suite's `sort_keys`; `heap` has no layout
  to order and gets the bare `USING heap`, so it also works against stock
  PostgreSQL over `--url`. An unknown name is refused before any table is
  dropped, so a typo cannot cost a loaded dataset.
- `--url CONNINFO` — benchmark an external server (e.g. stock PostgreSQL) for
  a side-by-side number. The dataset is loaded there the same way. Point it at
  a scratch database: a suite owns its table names outright, and `tpch`'s are
  `orders`, `customer`, `lineitem` — with `--reload` it drops them, listing
  what it drops first.
- `--query 1,5,29` — run only those query numbers; `--runs N` sets the
  repetitions (default 3: run 1 cold, the rest warm).
- `--timeout SECONDS` — per-run limit (default 120). Exceeding it abandons the
  connection, because the server has no query cancellation yet — the abandoned
  query keeps burning CPU behind the remaining ones, so treat a run with
  timeouts as indicative only.
- `--json PATH` — also write the results as JSON. `result` is positional, so
  `query_numbers` alongside it names the query each slot belongs to; a
  `--query`-filtered run would otherwise read as Q1..Qn.

## Adding a benchmark

Drop the benchmark's `create.sql` and `queries.sql` under `suites/<name>/`,
staying as close to the source text as the benchmark's own licensing allows,
and add a `Suite` to `src/suites.rs`. Record the provenance in the repo's
`NOTICE` — where the SQL came from, and whether it is a copy or a
transcription.

- `tables` lists the tables in load order, one per `CREATE TABLE` in
  `create.sql`. More than one of them makes `--data` a directory of per-table
  files rather than a single file.
- `sort_keys` gives each table its layout sort key, one entry per `tables` entry
  and in the same order — `"a"` or `"a, b"`, naming columns that exist in that
  table and never repeating one. It lives here rather than in `create.sql`
  because the vendored DDL is kept byte-identical to upstream, and because the
  clause has to follow `USING <am>`, which is spliced on at the end. Only an
  engine-managed method uses it; a heap run ignores it.
- `QueryFormat::OnePerLine` is how ClickBench publishes its queries, and keeps
  the numbering matched to the published results by position.
  `QueryFormat::Numbered` is for multi-line queries, each under a `-- Qn`
  marker that is where its number comes from — so commenting one out does not
  renumber the rest. `-- Qn setup` and `-- Qn teardown` sections run outside
  the timed window, and teardown runs even when the query failed.
- `DataFormat` picks the `COPY`: `Tsv` (PostgreSQL text escaping), `Csv` (with
  a header the loader strips itself), or `Psv` (pipe-delimited, no header, one
  optional trailing delimiter per line).
- `dataset_hint` is printed verbatim when there is no data, so make it a
  runnable recipe rather than a URL.

`create.sql` is split on `;` and each statement is checked to be the
`CREATE TABLE` for the table `tables` names at that position. That is what
makes the split safe — a `;` inside a comment or a string literal cuts a
statement in half, and the check catches the wreckage instead of executing it.
It also catches a schema that has drifted from `tables`. `--using` then splices
`USING <am>` onto each statement — followed by `ORDER BY (<sort key>)` when the
method requires one — and refuses if the statement ends in a comment, which would
swallow the clause and leave the run quietly measuring the default heap.
