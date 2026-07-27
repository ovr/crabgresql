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

| Suite | Table | Queries | Dataset |
| --- | --- | --- | --- |
| `clickbench` | `hits` (105 columns) | 43 | [hits.tsv.gz](https://datasets.clickhouse.com/hits_compatible/hits.tsv.gz), 100M rows / ~70 GB uncompressed |

`bench list` prints the same thing.

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
 28     0.294     0.295     0.296     0.294       679  ok
 29         -         -         -         -         -  ERROR: function regexp_replace(text, unknown, unknown) does not exist
 30     0.655     0.649     0.659     0.649         1  ok

42 of 43 queries succeeded, 12.341s total (best runs)
```

Build in `--release`; a debug build measures the compiler, not the engine.

The load streams in as a sequence of `COPY` batches, because the server
materializes a whole `COPY` payload before inserting any of it — one statement
for the full file would OOM the (in-process) server long before the engine's
own limits mattered. Even so, the loaded table has to fit in the machine, so
take a slice with `--rows` until the engine grows out-of-core execution.

## Options that matter

- `--rows N` — load only the first N rows of `--data`.
- `--data-dir DIR` — put the server's data directory somewhere persistent.
  A later run over the same directory sees the table already loaded and skips
  the load entirely; `--reload` forces a rebuild. A reused table's storage is
  reported as `unknown`, and `--using`/`--rows` are refused rather than
  silently ignored, since neither can be honored without rebuilding.
- `--using AM` — create the table with an access method (`parquet`, `buffer`),
  instead of the default heap. Every report names the storage its numbers were
  measured on.
- `--url CONNINFO` — benchmark an external server (e.g. stock PostgreSQL) for
  a side-by-side number. The table is loaded there the same way.
- `--query 1,5,29` — run only those query numbers; `--runs N` sets the
  repetitions (default 3: run 1 cold, the rest warm).
- `--timeout SECONDS` — per-run limit (default 120). Exceeding it abandons the
  connection, because the server has no query cancellation yet — the abandoned
  query keeps burning CPU behind the remaining ones, so treat a run with
  timeouts as indicative only.
- `--json PATH` — also write the results as JSON.

## Adding a benchmark

Drop the upstream `create.sql` and `queries.sql` (one query per line, so the
numbering matches the published results) under `suites/<name>/`, keeping them
byte-identical to their source, and add a `Suite` to `src/suites.rs`. Record
the provenance in the repo's `NOTICE`. If the data is not a
PostgreSQL-text-escaped TSV, `DataFormat` also has `Csv`.
