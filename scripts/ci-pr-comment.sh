#!/usr/bin/env bash
# Upserts a pull request comment identified by an HTML marker.
#
# Every CI job that reports into a pull request wants the same thing: one
# comment per report, edited in place instead of a new one per push. The marker
# is what makes it findable, so it has to be inside the body — keep it on the
# first line of BODY_FILE.
#
# Usage: scripts/ci-pr-comment.sh <marker> <body-file>
# Needs: GH_TOKEN, GITHUB_REPOSITORY, PR (the pull request number).
set -euo pipefail

if [ $# -ne 2 ]; then
    echo "usage: $0 <marker> <body-file>" >&2
    exit 2
fi
marker=$1
body=$2

if ! grep -qF "$marker" "$body"; then
    echo "$0: $body does not contain the marker $marker" >&2
    exit 2
fi

existing=$(gh api "repos/$GITHUB_REPOSITORY/issues/$PR/comments" --paginate \
    --jq ".[] | select(.body | contains(\"$marker\")) | .id" \
    | head -n 1)

if [ -n "$existing" ]; then
    gh api --method PATCH "repos/$GITHUB_REPOSITORY/issues/comments/$existing" \
        -F body=@"$body" > /dev/null
    echo "updated comment $existing"
else
    gh api --method POST "repos/$GITHUB_REPOSITORY/issues/$PR/comments" \
        -F body=@"$body" > /dev/null
    echo "posted a new comment"
fi
