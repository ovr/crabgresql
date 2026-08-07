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
# The entries carry no suite prefix: the action keys its history by the `name`
# input of the step, renders one heading per key and one chart per entry name
# inside it, so the suite is named there and `Q1` is enough here.
#
# A missing input yields `[]` rather than an error: the report job runs even
# when one of the benchmark jobs died, and the other suites still have numbers.
#
# Usage: scripts/bench/trend-json.sh <bench.json>
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: $0 <bench.json>" >&2
    exit 2
fi
file=$1

if [ ! -s "$file" ]; then
    echo '[]'
    exit 0
fi

jq -c '
  "\(.access_method), \(.rows) rows" as $context
  |
  [ .load_time // empty
    | { name: "load", unit: "s", value: ., extra: "dataset load — \($context)" }
  ]
  +
  [ range(0; .result | length) as $i
    | { n: .query_numbers[$i],
        best: ([.result[$i][] | select(. != null)] | min) }
    | select(.best != null)
    | { name: ("Q" + (.n | tostring)), unit: "s", value: .best,
        extra: "best of the timed runs — \($context)" }
  ]
' "$file"
