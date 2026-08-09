# Releasing

A release is a pushed tag and nothing else. `.github/workflows/release.yml`
does the rest: it builds the binary once per architecture on a native runner,
attaches the tarballs to the GitHub Release for that tag, and publishes a
multi-arch image to `ovr/crabgresql` on Docker Hub. Creating the Release is not
its job — it expects one to already exist for the tag.

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
uploaded or pushed. A version with a suffix — `v0.2.0-rc1` — is treated as a
prerelease: it is marked as one on GitHub and does not move the `latest` image
tag.

## What gets published

| Artifact | Where |
| --- | --- |
| `ovr/crabgresql:{X.Y.Z, X.Y, X, latest}` | Docker Hub, `linux/amd64` + `linux/arm64` |
| `crabgresql-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | GitHub Release |
| `crabgresql-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` | GitHub Release |
| `crabgresql-vX.Y.Z-aarch64-apple-darwin.tar.gz` | GitHub Release |
| a `.tar.gz.sha256` beside each tarball | GitHub Release |

Linux tarballs are glibc builds from the GitHub runners, not static musl ones:
musl's allocator costs real throughput under a database workload, and the
portable answer is the image. macOS ships arm64 only.

The workflow does not create the GitHub Release — that happens separately when
the tag goes up. Each build leg uploads its own tarball into it the moment it
has one, so an architecture is downloadable while the others are still
compiling; if the Release does not exist yet, the upload step fails and the leg
can be re-run.

Each architecture's image is smoke-tested before it is pushed, and pushed by
digest without a tag; the tags are created once, on the merged manifest, so
`latest` is never briefly single-architecture. The `image` job then asserts
that both platforms are actually in the manifest — `imagetools create` is happy
to build a manifest with one.

## Repository secrets

| Secret | Used for |
| --- | --- |
| `DOCKERHUB_USERNAME` | Docker Hub login |
| `DOCKERHUB_TOKEN` | Docker Hub access token with write scope on `ovr/crabgresql` |

`GITHUB_TOKEN` covers uploading the assets; the `build` job requests
`contents: write` for it.

## When it fails

- **`verify` fails.** The tag and `Cargo.toml` disagree. Nothing was published.
  Delete the tag (`git push --delete origin vX.Y.Z`), fix the version, tag
  again.
- **One `build` matrix leg fails.** The image manifest is never created, and
  the Release is missing that architecture's tarball. The other architecture's
  image is already on Docker Hub as an untagged digest — harmless, and
  garbage-collected. Fix and re-run the workflow: the digest push is
  idempotent and the uploads use `--clobber`.

Re-running a release for the *same* version overwrites the image tags. Prefer a
new patch version over re-cutting one people may already have pulled.
