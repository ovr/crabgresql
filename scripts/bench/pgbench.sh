#!/usr/bin/env bash
# Runs PostgreSQL's own pgbench against a real crabgresql server process.
#
# This is the only benchmark here that cannot use crabgresql-bench's in-process
# server: pgbench is an external libpq client, so it needs a socket and a
# server it can outlive. The workload is OLTP — it measures the buffer pool,
# the WAL and MVCC, which the analytical suites do not touch.
#
# Usage: scripts/bench/pgbench.sh <datadir> <port> <outdir>
set -euo pipefail

SCALE=10
CLIENTS=4
THREADS=4
DURATION=60

# The -s 10 working set is ~178 MB against a 128 MB default, and this number
# moves more with the pool size than with most engine changes — so pin it and
# print it, rather than measuring whatever the default happens to be today.
export CRABGRESQL_SHARED_BUFFERS=${CRABGRESQL_SHARED_BUFFERS:-2GB}

if [ $# -ne 3 ]; then
    echo "usage: $0 <datadir> <port> <outdir>" >&2
    exit 2
fi
datadir=$1
port=$2
outdir=$3

# Resolve the arguments against the caller's directory before moving to the
# repository root, so a relative path means what the caller meant.
mkdir -p "$datadir" "$outdir"
datadir=$(cd "$datadir" && pwd)
outdir=$(cd "$outdir" && pwd)

cd "$(dirname "$0")/../.."
server=target/release/crabgresql
if [ ! -x "$server" ]; then
    echo "$0: $server not built — cargo build --release -p crabgresql-server --bin crabgresql" >&2
    exit 2
fi

# Debian and Ubuntu ship pgbench in postgresql-contrib, which does not put it
# on PATH; the Homebrew and PGDG layouts do.
pgbench=$(command -v pgbench || true)
if [ -z "$pgbench" ]; then
    pgbench=$(ls -1 /usr/lib/postgresql/*/bin/pgbench 2>/dev/null | sort -V | tail -n 1 || true)
fi
if [ -z "$pgbench" ]; then
    echo "$0: no pgbench found (Debian/Ubuntu: apt-get install postgresql-contrib)" >&2
    exit 2
fi

export PGHOST=127.0.0.1 PGPORT=$port PGUSER=postgres PGDATABASE=postgres

"$server" -D "$datadir" -p "$port" > "$outdir/server.log" 2>&1 &
srv=$!
# TERM, not KILL: the server flushes on the way out, and a half-written data
# directory would make the next step's failure look like an engine bug.
trap 'kill -TERM $srv 2>/dev/null || true; wait $srv 2>/dev/null || true' EXIT

for _ in $(seq 60); do
    if psql -qtAc 'SELECT 1' > /dev/null 2>&1; then
        break
    fi
    if ! kill -0 $srv 2>/dev/null; then
        echo "$0: the server exited during startup:" >&2
        cat "$outdir/server.log" >&2
        exit 1
    fi
    sleep 1
done
psql -qtAc 'SELECT 1' > /dev/null

echo "$($pgbench --version), shared_buffers=$CRABGRESQL_SHARED_BUFFERS"

# -I dtGvp rather than the default dtgvp: step `g` generates the rows
# client-side through `COPY … FREEZE`, which this server refuses outside a
# table created in the same transaction. `G` generates them server-side with
# INSERT … SELECT FROM generate_series instead. Step `f` (foreign keys) is
# left out because ALTER TABLE ADD CONSTRAINT … FOREIGN KEY is not supported.
# `-I` is only honored alongside `-i`, hence both.
echo "== init (-i -I dtGvp -s $SCALE)"
"$pgbench" -i -I dtGvp -s "$SCALE" 2>&1 | tee "$outdir/init.log"

echo "== tpcb-like (-c $CLIENTS -j $THREADS -T $DURATION)"
"$pgbench" -c "$CLIENTS" -j "$THREADS" -T "$DURATION" 2>&1 | tee "$outdir/tpcb.log"

echo "== read-only (-S -c $CLIENTS -j $THREADS -T $DURATION)"
"$pgbench" -S -c "$CLIENTS" -j "$THREADS" -T "$DURATION" 2>&1 | tee "$outdir/select.log"

# A run that reports failed transactions is a red run even though pgbench
# itself exits 0: the tps below it was earned by not finishing the work.
failed=0
for log in "$outdir/tpcb.log" "$outdir/select.log"; do
    n=$(sed -n 's/^number of failed transactions: \([0-9]*\).*/\1/p' "$log")
    failed=$((failed + ${n:-0}))
done

tps() { sed -n 's/^tps = \([0-9.]*\).*/\1/p' "$1" | tail -n 1; }
{
    echo "### pgbench — heap, scale $SCALE, ${CLIENTS} clients, ${DURATION}s"
    echo
    echo "\`$($pgbench --version)\`, \`CRABGRESQL_SHARED_BUFFERS=$CRABGRESQL_SHARED_BUFFERS\`"
    echo
    echo '| workload | tps | failed |'
    echo '| --- | ---: | ---: |'
    echo "| tpcb-like (\`-c $CLIENTS -j $THREADS -T $DURATION\`) | $(tps "$outdir/tpcb.log") | $(sed -n 's/^number of failed transactions: \([0-9]*\).*/\1/p' "$outdir/tpcb.log") |"
    echo "| read-only (\`-S\`) | $(tps "$outdir/select.log") | $(sed -n 's/^number of failed transactions: \([0-9]*\).*/\1/p' "$outdir/select.log") |"
    echo
} > "$outdir/pgbench.md"
cat "$outdir/pgbench.md"

if [ "$failed" -ne 0 ]; then
    echo "$0: $failed failed transactions" >&2
    exit 1
fi
