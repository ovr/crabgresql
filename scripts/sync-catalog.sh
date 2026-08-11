#!/usr/bin/env bash
# Syncs PostgreSQL's system-catalog *data* files (src/include/catalog/*.dat)
# into vendor/postgres/catalog/. These BKI data files seed pg_catalog's built-in
# rows (pg_type, pg_proc, pg_cast, pg_namespace); crabgresql-bki codegens from
# them at build time, and crabgresql-catalog includes what it emits.
#
# Only the .dat DATA is vendored — never PostgreSQL source code or the Perl
# Catalog.pm parser; see docs/ARCHITECTURE.md §7 and vendor/postgres/README.md.
# The pin matches the regress corpus (vendor/postgres/REGRESS_COMMIT).
set -euo pipefail

cd "$(dirname "$0")/.."
COMMIT=$(cat vendor/postgres/REGRESS_COMMIT)

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The catalogs crabgresql-bki currently reads (see its `generate`). Add more
# .dat names here as additional pg_catalog relations gain built-in-row support.
# TODO: codegen pg_namespace's built-in rows from the vendored
# pg_namespace.dat — crabgresql-bki never reads that file, so
# crabgresql-catalog spells the rows out by hand.
DATS=(pg_type pg_proc pg_cast pg_namespace)

echo "Fetching postgres catalog data @$COMMIT ..."
paths=()
for d in "${DATS[@]}"; do
    paths+=("postgres-$COMMIT/src/include/catalog/$d.dat")
done
curl -fsSL "https://codeload.github.com/postgres/postgres/tar.gz/$COMMIT" -o "$tmp/pg.tar.gz"
tar -xzf "$tmp/pg.tar.gz" -C "$tmp" "${paths[@]}"

rm -rf vendor/postgres/catalog
mkdir -p vendor/postgres/catalog
for d in "${DATS[@]}"; do
    cp "$tmp/postgres-$COMMIT/src/include/catalog/$d.dat" vendor/postgres/catalog/
done
printf '%s\n' "$COMMIT" > vendor/postgres/CATALOG_COMMIT

echo "Synced ${#DATS[@]} catalog .dat files at $COMMIT"
