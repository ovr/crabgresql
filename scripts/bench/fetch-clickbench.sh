#!/usr/bin/env bash
# Downloads a slice of the ClickBench `hits` dataset as uncompressed TSV.
#
# The published file is 100M rows / ~70 GB uncompressed, which no CI runner
# holds. gzip is a stream, so a range request decompresses cleanly up to the
# cut — we ask for a few more bytes than the slice needs and let `head` stop
# the pipe. See crates/crabgresql-bench/README.md for the manual recipe.
#
# Usage: scripts/bench/fetch-clickbench.sh <rows> <out.tsv>
set -eu # NOT pipefail: see the SIGPIPE note below.

URL=https://datasets.clickhouse.com/hits_compatible/hits.tsv.gz

# Measured on the published file: ~155 MB of gzip per million rows over the
# first 200k. The margin is because the ratio drifts with the data, and a short
# read shows up as a truncated last line rather than a clean error.
BYTES_PER_MILLION_ROWS=160000000
MARGIN=1.15

if [ $# -ne 2 ]; then
    echo "usage: $0 <rows> <out.tsv>" >&2
    exit 2
fi
rows=$1
out=$2

bytes=$(awk -v r="$rows" -v b="$BYTES_PER_MILLION_ROWS" -v m="$MARGIN" \
    'BEGIN { printf "%d", r / 1000000 * b * m }')

echo "Fetching the first $rows rows of hits.tsv (~$((bytes / 1000000)) MB gzipped) ..."
echo "(curl error 56 below is the expected SIGPIPE; the row count at the end is the check)"

# `head` closes the pipe once it has its rows, so curl and gzip die of SIGPIPE
# (141) on the *successful* path — that is why pipefail is off and why the
# check below is on the row count rather than on an exit status. gzip also
# complains about the truncated stream; that message is the expected one.
curl -sS -r "0-$bytes" "$URL" | gzip -dc 2>/dev/null | head -n "$rows" > "$out"

got=$(wc -l < "$out" | tr -d ' ')
if [ "$got" -ne "$rows" ]; then
    echo "$0: wanted $rows rows, got $got — raise the byte estimate" >&2
    exit 1
fi

echo "Wrote $out ($(du -h "$out" | cut -f1), $got rows)"
