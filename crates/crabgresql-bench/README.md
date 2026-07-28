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
$ cargo run --release -p crabgresql-bench -- run clickbench --data hits.tsv
...
 34     0.293     0.293        10  ok
 35     0.302     0.302        10  ok
 36     0.296     0.296        10  ok

43 of 43 queries succeeded, 13.228s total (best runs)
```

Build in `--release`; a debug build measures the compiler, not the engine.

## Running TPC-H

There is nothing to download: TPC-H data is generated. DuckDB's `tpch`
extension is the least ceremony, and unlike TPC's own `dbgen` it writes no
trailing `|` on each line — which matters, because a trailing delimiter is an
extra empty column as far as `COPY` is concerned.

```console
$ duckdb tpch.duckdb -c "INSTALL tpch; LOAD tpch; CALL dbgen(sf=1);"
$ mkdir -p tpch
$ for t in region nation part supplier partsupp customer orders lineitem; do
    duckdb tpch.duckdb -c "COPY $t TO 'tpch/$t.tbl' (FORMAT csv, DELIMITER '|', HEADER false);"
  done
```

`--data` is then the *directory*, not a file — the suite has eight tables, and
each is loaded from `<dir>/<table>.tbl` (`.csv` is accepted too, so a bulk
`EXPORT DATABASE` works unrenamed):

```console
$ cargo run --release -p crabgresql-bench -- run tpch --data tpch/
...
 19         -         -         -  timed out
 20    10.889    10.889         1  ok

21 of 22 queries succeeded, 40.450s total (best runs)
```

The queries are the specification's own text — implicit comma joins, no
hand-rewriting into `JOIN … ON`. Rewriting them would measure the rewrite
rather than the planner, and a query the planner cannot yet handle is exactly
what this harness exists to surface: Q19's whole `WHERE` is one top-level `OR`,
so there is no conjunct for the pushdown pass to extract into a join condition,
and it degenerates into a cross product.

Start small. TPC-H is quadratic in the wrong places for a young planner, so
`sf=0.01` is a sensible first run and `sf=1` is not.

## How the load works

The load streams in as a sequence of `COPY` batches, because the server
materializes a whole `COPY` payload before inserting any of it — one statement
for the full file would OOM the (in-process) server long before the engine's
own limits mattered. Even so, the loaded data has to fit in the machine, so
take a slice with `--rows` until the engine grows out-of-core execution.

## Options that matter

- `--rows N` — load only the first N rows of each table's data file.
- `--data-dir DIR` — put the server's data directory somewhere persistent.
  A later run over the same directory sees the tables already loaded and skips
  the load entirely; `--reload` forces a rebuild. A reused dataset's storage is
  reported as `unknown`, and `--using`/`--rows` are refused rather than
  silently ignored, since neither can be honored without rebuilding. A dataset
  that is only partly there is refused too, rather than half-loaded.
- `--using AM` — create the tables with an access method (`parquet`, `buffer`),
  instead of the default heap. Every report names the storage its numbers were
  measured on.
- `--url CONNINFO` — benchmark an external server (e.g. stock PostgreSQL) for
  a side-by-side number. The dataset is loaded there the same way.
- `--query 1,5,29` — run only those query numbers; `--runs N` sets the
  repetitions (default 3: run 1 cold, the rest warm).
- `--timeout SECONDS` — per-run limit (default 120). Exceeding it abandons the
  connection, because the server has no query cancellation yet — the abandoned
  query keeps burning CPU behind the remaining ones, so treat a run with
  timeouts as indicative only.
- `--json PATH` — also write the results as JSON.

## Adding a benchmark

Drop the upstream `create.sql` and `queries.sql` under `suites/<name>/`, keeping
them byte-identical to their source, and add a `Suite` to `src/suites.rs`.
Record the provenance in the repo's `NOTICE`.

- `tables` lists the tables in load order, one per `CREATE TABLE` in
  `create.sql`. More than one of them makes `--data` a directory of per-table
  files rather than a single file.
- `QueryFormat::OnePerLine` is how ClickBench publishes its queries, and keeps
  the numbering matched to the published results by position.
  `QueryFormat::Numbered` is for multi-line queries, each under a `-- Qn`
  marker that is where its number comes from — so commenting one out does not
  renumber the rest.
- `DataFormat` picks the `COPY`: `Tsv` (PostgreSQL text escaping), `Csv` (with
  a header the loader strips itself), or `Psv` (pipe-delimited, no header).

`--using` splices `USING <am>` onto every statement in `create.sql`, so that
file must hold bare `CREATE TABLE`s and nothing else — a comment would swallow
the clause and the run would quietly measure the default heap, so the splicer
refuses rather than lie about the storage.
