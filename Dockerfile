# syntax=docker/dockerfile:1

# Two ways in, one image out.
#
#   docker build .                                   # compile from source
#   docker build --build-arg BIN_SOURCE=prebuilt .   # package ./crabgresql
#
# The release workflow already compiles the binary on a native runner of each
# architecture (arm64 under QEMU would take hours), so it takes the second
# route and feeds that binary in. Both routes share the runtime stage below,
# which is the point of doing it with an argument instead of a second file:
# whatever CI proves about the image it builds from source holds for the image
# that ships.
ARG BIN_SOURCE=build

# Pinned to the exact channel in `rust-toolchain.toml`. Drifting from it means
# rustup downloads a second toolchain inside the build, which is slow and
# silently untested.
FROM rust:1.93.0-slim-trixie AS build

WORKDIR /src
COPY . .

# The cache mounts survive across builds but not into the image, so the binary
# has to be copied out of `target/` inside the same RUN that produced it.
# `--locked` because a release image resolving a different dependency set than
# CI tested is the whole problem lockfiles exist to prevent.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release -p crabgresql-server --bin crabgresql --locked \
    && cp target/release/crabgresql /crabgresql \
    && strip /crabgresql

# Unused — and therefore never built — unless BIN_SOURCE says otherwise, so a
# plain `docker build .` does not need a `crabgresql` in the context.
FROM scratch AS prebuilt
COPY crabgresql /crabgresql

FROM ${BIN_SOURCE} AS binary

# Debian rather than distroless: this is a database, and the first thing anyone
# does to a database that misbehaves is `docker exec` into it. No extra
# packages are needed — the build pulls in no C libraries, and `jiff` bundles
# the timezone database into the binary, so there is no `tzdata` to install.
FROM debian:trixie-slim

# A fixed uid/gid so a bind-mounted data directory has a stable owner across
# hosts. 5433 to match the port, which is one above PostgreSQL's.
RUN groupadd --system --gid 5433 crabgresql \
    && useradd --system --uid 5433 --gid 5433 --home-dir /var/lib/crabgresql \
       --shell /usr/sbin/nologin crabgresql \
    && mkdir -p /var/lib/crabgresql \
    && chown crabgresql:crabgresql /var/lib/crabgresql

COPY --from=binary /crabgresql /usr/local/bin/crabgresql
COPY LICENSE NOTICE /usr/share/doc/crabgresql/

# `0.0.0.0` is the one setting that differs from a local build, and it is a
# security decision: authentication is trust and the connection is cleartext,
# so a published port is a superuser account. Run this on a trusted network, or
# override CRABGRESQL_LISTEN_ADDRESS.
ENV PGDATA=/var/lib/crabgresql \
    CRABGRESQL_LISTEN_ADDRESS=0.0.0.0 \
    CRABGRESQL_PORT=5433

VOLUME /var/lib/crabgresql
EXPOSE 5433
# Numeric, so an orchestrator enforcing `runAsNonRoot` can tell this is not
# root without resolving a name out of the image's /etc/passwd.
USER 5433:5433
WORKDIR /var/lib/crabgresql

# No HEALTHCHECK: the image ships no client, and pulling in postgresql-client
# to run `pg_isready` would cost more than the check is worth. Probe the port
# from outside, or from a sidecar that already has psql.

# Exec form on purpose. Under a shell the server would not be PID 1, would
# never see SIGTERM, and `docker stop` would end in SIGKILL — which leaves the
# control file dirty and resets unlogged tables on the next start.
STOPSIGNAL SIGTERM
ENTRYPOINT ["crabgresql"]
