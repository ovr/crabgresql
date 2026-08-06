#!/usr/bin/env bash
# Wraps a crabgresql-bench results table in markdown.
#
# The harness already prints a fixed-width table; a fenced block keeps its
# columns aligned in a pull request comment, where a markdown table of 43 rows
# would not be readable anyway.
#
# Usage: scripts/bench/report.sh <heading> <results.txt>
set -euo pipefail

if [ $# -ne 2 ]; then
    echo "usage: $0 <heading> <results.txt>" >&2
    exit 2
fi

echo "### $1"
echo
if [ -s "$2" ]; then
    echo '```'
    cat "$2"
    echo '```'
else
    echo '⚠️ produced no output — see the job log.'
fi
echo
