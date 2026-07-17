#!/usr/bin/env bash
# Syncs the PostgreSQL regression test corpus into vendor/postgres/regress.
#
# Only the test data is vendored (sql/, expected/, data/, parallel_schedule,
# resultmap) — never PostgreSQL source code; see docs/ARCHITECTURE.md §7 and
# vendor/postgres/README.md. To bump the pin: edit COMMIT, rerun, commit.
set -euo pipefail

COMMIT=03480907e9ff5d9bb3296b56c9b49db3df756e0f # postgres master, 2026-07-17

cd "$(dirname "$0")/.."

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Fetching postgres@$COMMIT ..."
curl -fsSL "https://codeload.github.com/postgres/postgres/tar.gz/$COMMIT" -o "$tmp/pg.tar.gz"
tar -xzf "$tmp/pg.tar.gz" -C "$tmp" \
    "postgres-$COMMIT/src/test/regress/sql" \
    "postgres-$COMMIT/src/test/regress/expected" \
    "postgres-$COMMIT/src/test/regress/data" \
    "postgres-$COMMIT/src/test/regress/parallel_schedule" \
    "postgres-$COMMIT/src/test/regress/resultmap" \
    "postgres-$COMMIT/COPYRIGHT"

rm -rf vendor/postgres/regress
mkdir -p vendor/postgres/regress
cp -R "$tmp/postgres-$COMMIT/src/test/regress/sql" \
      "$tmp/postgres-$COMMIT/src/test/regress/expected" \
      "$tmp/postgres-$COMMIT/src/test/regress/data" \
      vendor/postgres/regress/
cp "$tmp/postgres-$COMMIT/src/test/regress/parallel_schedule" \
   "$tmp/postgres-$COMMIT/src/test/regress/resultmap" \
   vendor/postgres/regress/
cp "$tmp/postgres-$COMMIT/COPYRIGHT" vendor/postgres/COPYRIGHT
printf '%s\n' "$COMMIT" > vendor/postgres/REGRESS_COMMIT

echo "Synced $(ls vendor/postgres/regress/sql | wc -l | tr -d ' ') sql files at $COMMIT"
