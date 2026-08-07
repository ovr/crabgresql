#!/usr/bin/env bash
# Converts a crabgresql-bench --json report into github-action-benchmark's
# `customSmallerIsBetter` shape: a flat array of {name, unit, value, extra}.
#
# One entry per query plus one for the load, so the trend chart says *which*
# query moved rather than only that the suite got slower. The value is the best
# run, matching what the results table reports; a query that failed contributes
# no entry at all, because a zero would read as infinitely fast and a
# placeholder would read as a real measurement.
#
# A missing input yields `[]` rather than an error: the report job runs even
# when one of the benchmark jobs died, and the other suites still have numbers.
#
# Usage: scripts/bench/trend-json.sh <label> <bench.json>
set -euo pipefail

if [ $# -ne 2 ]; then
    echo "usage: $0 <label> <bench.json>" >&2
    exit 2
fi
label=$1
file=$2

if [ ! -s "$file" ]; then
    echo '[]'
    exit 0
fi

jq -c --arg label "$label" '
  [ .load_time // empty
    | { name: ($label + " — load"), unit: "s", value: .,
        extra: "\($label) dataset load" }
  ]
  +
  [ range(0; .result | length) as $i
    | { n: .query_numbers[$i],
        best: ([.result[$i][] | select(. != null)] | min) }
    | select(.best != null)
    | { name: ($label + " — Q" + (.n | tostring)), unit: "s", value: .best,
        extra: "best of the timed runs" }
  ]
' "$file"
