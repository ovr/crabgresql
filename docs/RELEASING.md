# Releasing

A release is a pushed tag and nothing else. `.github/workflows/release.yml`
does the rest: it builds the binary once per architecture on a native runner,
attaches the tarballs to a GitHub Release, and publishes a multi-arch image to
`ovr/crabgresql` on Docker Hub.

## Cutting one

1. Raise `version` under `[workspace.package]` in the root `Cargo.toml`. Every
   crate inherits it with `version.workspace = true`, so that is the only edit.
2. `cargo check --workspace --locked` — this rewrites `Cargo.lock`, which has
   to be part of the same commit or the release build fails on `--locked`.
3. Commit, merge to `main`.
4. Tag the merged commit and push it:

   ```console
   $ git tag v0.2.0
   $ git push origin v0.2.0
   ```

The tag must be `v` followed by exactly the `Cargo.toml` version; the `verify`
job compares them and stops the release if they disagree, before anything is
published. A version with a suffix — `v0.2.0-rc1` — is treated as a
prerelease: it is marked as one on GitHub and does not move the `latest` image
tag.

## What gets published

| Artifact | Where |
| --- | --- |
| `ovr/crabgresql:{X.Y.Z, X.Y, X, latest}` | Docker Hub, `linux/amd64` + `linux/arm64` |
| `crabgresql-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | GitHub Release |
| `crabgresql-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` | GitHub Release |
| `crabgresql-vX.Y.Z-aarch64-apple-darwin.tar.gz` | GitHub Release |
| `SHA256SUMS` | GitHub Release |

Linux tarballs are glibc builds from the GitHub runners, not static musl ones:
musl's allocator costs real throughput under a database workload, and the
portable answer is the image. macOS ships arm64 only.

Each architecture's image is smoke-tested (`scripts/docker-smoke.sh`) before it
is pushed, and pushed by digest without a tag; the tags are created once, on
the merged manifest, so `latest` is never briefly single-architecture. The
`image` job then asserts that both platforms are actually in the manifest —
`imagetools create` is happy to build a manifest with one.

## Repository secrets

| Secret | Used for |
| --- | --- |
| `DOCKERHUB_USERNAME` | Docker Hub login |
| `DOCKERHUB_TOKEN` | Docker Hub access token with write scope on `ovr/crabgresql` |

`GITHUB_TOKEN` covers the GitHub Release; the job requests `contents: write`
for it.

## When it fails

- **`verify` fails.** The tag and `Cargo.toml` disagree. Nothing was published.
  Delete the tag (`git push --delete origin vX.Y.Z`), fix the version, tag
  again.
- **One `build` matrix leg fails.** The manifest is never created and no tag
  points at the release, but the other architecture's image is already on
  Docker Hub as an untagged digest — harmless, and garbage-collected. Fix and
  re-run the workflow; the digest push is idempotent.
- **`release` fails after `image` succeeded.** The image tags exist without a
  GitHub Release. Re-running the job is safe: `gh release create` on an
  existing tag is the only step, and it can be retried once the release is
  deleted.

Re-running a release for the *same* version overwrites the image tags. Prefer a
new patch version over re-cutting one people may already have pulled.
