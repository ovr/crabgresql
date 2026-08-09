# syntax=docker/dockerfile:1

# `docker build .` compiles from source; `--build-arg BIN_SOURCE=prebuilt`
# packages ./crabgresql instead, which is how the release workflow reuses the
# binary it already built on a native runner of each architecture.
ARG BIN_SOURCE=build

# The channel in rust-toolchain.toml; anything else makes rustup download a
# second toolchain inside the build.
FROM rust:1.93.0-slim-trixie AS build

WORKDIR /src
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release -p crabgresql-server --bin crabgresql --locked \
    && cp target/release/crabgresql /crabgresql \
    && strip /crabgresql

FROM scratch AS prebuilt
COPY crabgresql /crabgresql

FROM ${BIN_SOURCE} AS binary

FROM debian:trixie-slim

# 999 is where Debian starts allocating system uids downward, and what the
# official postgres image pins for the same reason.
RUN groupadd --system --gid 999 crabgresql \
    && useradd --system --uid 999 --gid 999 --home-dir /var/lib/crabgresql \
       --shell /usr/sbin/nologin crabgresql \
    && mkdir -p /var/lib/crabgresql \
    && chown crabgresql:crabgresql /var/lib/crabgresql

COPY --from=binary /crabgresql /usr/local/bin/crabgresql
COPY LICENSE NOTICE /usr/share/doc/crabgresql/

# 0.0.0.0 is a security decision: authentication is trust and the connection is
# cleartext, so a published port is a superuser account.
ENV PGDATA=/var/lib/crabgresql \
    CRABGRESQL_LISTEN_ADDRESS=0.0.0.0 \
    CRABGRESQL_PORT=5433

VOLUME /var/lib/crabgresql
EXPOSE 5433
USER crabgresql
WORKDIR /var/lib/crabgresql

# Exec form: under a shell the server would never see SIGTERM, and `docker
# stop` would end in SIGKILL, which leaves the control file dirty. SIGTERM
# rather than the postgres image's SIGINT, because this server treats the two
# the same and has no smart-shutdown mode to avoid.
ENTRYPOINT ["crabgresql"]
