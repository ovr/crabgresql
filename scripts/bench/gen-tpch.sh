#!/usr/bin/env bash
# Generates the TPC-H dataset with DuckDB's `tpch` extension.
#
# TPC-H data is generated, not downloaded, so the scale factor is the only
# knob. The output is one `<table>.tbl` per table, pipe-delimited and without a
# header — the shape crabgresql-bench's `tpch` suite loads (`DataFormat::Psv`);
# it accepts that extension only, because a file named for another format would
# be read with the wrong delimiter.
#
# Usage: scripts/bench/gen-tpch.sh <scale-factor> <outdir>
set -euo pipefail

DUCKDB_VERSION=v1.1.3

if [ $# -ne 2 ]; then
    echo "usage: $0 <scale-factor> <outdir>" >&2
    exit 2
fi
sf=$1
outdir=$2

# Load order matters to the suite, not to dbgen; keep it the suite's.
TABLES="region nation part supplier partsupp customer orders lineitem"

duckdb=$(command -v duckdb || true)
if [ -z "$duckdb" ]; then
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64)  asset=duckdb_cli-linux-amd64.zip ;;
        Linux-aarch64) asset=duckdb_cli-linux-aarch64.zip ;;
        Darwin-*)      asset=duckdb_cli-osx-universal.zip ;;
        *) echo "$0: no DuckDB build known for $(uname -s)-$(uname -m); install duckdb yourself" >&2
           exit 2 ;;
    esac
    dir=$(mktemp -d)
    echo "Fetching DuckDB $DUCKDB_VERSION ($asset) ..."
    curl -fsSL "https://github.com/duckdb/duckdb/releases/download/$DUCKDB_VERSION/$asset" \
        -o "$dir/duckdb.zip"
    unzip -q "$dir/duckdb.zip" -d "$dir"
    duckdb="$dir/duckdb"
    chmod +x "$duckdb"
fi
echo "Using $($duckdb --version)"

mkdir -p "$outdir"
db=$(mktemp -u)/tpch.duckdb
mkdir -p "$(dirname "$db")"
trap 'rm -rf "$(dirname "$db")"' EXIT

echo "Generating TPC-H at sf=$sf ..."
"$duckdb" "$db" -c "INSTALL tpch; LOAD tpch; CALL dbgen(sf=$sf);"
for t in $TABLES; do
    "$duckdb" "$db" -c \
        "COPY $t TO '$outdir/$t.tbl' (FORMAT csv, DELIMITER '|', HEADER false);"
done

echo "Wrote $outdir:"
for t in $TABLES; do
    printf '  %-10s %10s rows\n' "$t" "$(wc -l < "$outdir/$t.tbl" | tr -d ' ')"
done
