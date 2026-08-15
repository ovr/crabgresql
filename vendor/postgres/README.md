# PostgreSQL regression test corpus

This directory holds a vendored copy of PostgreSQL's regression test **data**
(`src/test/regress/`): `sql/`, `expected/`, `data/`, `parallel_schedule` and
`resultmap`. No PostgreSQL source code is vendored — only the test corpus, as
permitted by the clean-room policy in `docs/ARCHITECTURE.md` §7, with
attribution in the repo-root `NOTICE` file. The upstream license is in
`COPYRIGHT` (the PostgreSQL License).

It also holds `catalog/` — PostgreSQL's system-catalog **data** files
(`src/include/catalog/*.dat`: `pg_type`, `pg_proc`, `pg_cast`, `pg_namespace`,
`pg_opclass`, `pg_opfamily`).
These seed `pg_catalog`'s built-in rows: `crates/crabgresql-bki` codegens from
them at build time, and `crates/crabgresql-catalog` includes what it emits. Only the `.dat` DATA is vendored — never the C headers
or the Perl `Catalog.pm` parser. The pin is recorded in `CATALOG_COMMIT`
(kept equal to `REGRESS_COMMIT`).

To (re)populate or bump the catalog data:

```sh
./scripts/sync-catalog.sh   # reads the pin from REGRESS_COMMIT
```

## Provenance

The files come from the `postgres/postgres` GitHub mirror at the commit
recorded in `REGRESS_COMMIT` (postgres master / 19devel — matching the
`server_version = 19.0` CrabgreSQL reports).

To (re)populate or bump the pin:

```sh
./scripts/sync-regress.sh   # edit COMMIT inside the script to bump
```

The sync is reproducible: it downloads the codeload tarball for the pinned
commit and extracts only the paths above. `vendor/postgres/regress/` is fully
regenerated on every run, so local edits to it are always lost — the corpus is
used byte-for-byte unmodified.

## Notes on the corpus

- Since PostgreSQL 16 there is no `input//output/` `.source` template
  machinery; COPY-family tests locate `data/` with `\getenv abs_srcdir` and
  `COPY … FROM :'filename'` instead. The runner implements `\getenv`, `\set`,
  `\unset` and `:var` / `:'var'` / `:"var"` interpolation, and the server reads
  the named file itself; other metacommands still emit a deterministic stub
  line, so tests that need them cannot pass yet.
- `serial_schedule` was removed upstream in PostgreSQL 14; `parallel_schedule`
  is the only schedule, and our runner executes its groups serially in file
  order (as `pg_regress` itself supports).
- `resultmap` only maps platform-specific expected variants (cygwin/mingw
  float4); the runner ignores it and instead tries the `<test>_1.out` …
  `<test>_9.out` variant convention directly.

The runner lives in `crates/crabgresql-pg-regress`; see the repo README for
usage.
