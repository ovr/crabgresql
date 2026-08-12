#!/usr/bin/env bash
# Regenerate CHANGELOG.md for the version `cargo release` is about to tag.
#
# `cargo release` runs its pre-release hook once per crate, with the working
# directory set to that crate's root — not the workspace root. git-cliff
# resolves both its config and its repository relative to the working
# directory, so calling it directly from the hook fails with "could not find
# repository from crates/<crate>". This script pins both to $WORKSPACE_ROOT,
# which `cargo release` exports for exactly this kind of hook.
#
# Rewriting the same file once per crate is wasteful but harmless: git-cliff
# regenerates the whole changelog from the history every time, so the result is
# identical no matter how often it runs.
set -euo pipefail

root="${WORKSPACE_ROOT:?WORKSPACE_ROOT is set by cargo release; run this from the hook}"
version="${NEW_VERSION:?NEW_VERSION is set by cargo release}"

# A dry run must leave the tree untouched, but still exercise the generation so
# a broken cliff.toml surfaces before `--execute`.
out="$root/CHANGELOG.md"
if [ "${DRY_RUN:-false}" = "true" ]; then
    out=/dev/null
fi

exec git-cliff \
    --repository "$root" \
    --config "$root/cliff.toml" \
    --tag "v$version" \
    --output "$out"
