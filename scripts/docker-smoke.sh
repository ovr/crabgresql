#!/usr/bin/env bash
# Checks that an image actually serves SQL, keeps a volume across a restart,
# and shuts down cleanly.
#
# Everything here is a property of the image rather than of the engine — the
# port is reachable from outside the container, the data directory is writable
# by the unprivileged user it runs as, the volume outlives the container, and
# PID 1 receives SIGTERM so `docker stop` is a clean shutdown rather than a
# crash. Each of those has a plausible way to be silently wrong (a loopback
# bind, a root-owned PGDATA, a shell-form ENTRYPOINT), and none of them shows
# up in `cargo test`.
#
# Usage: scripts/docker-smoke.sh <image> [port]
set -euo pipefail

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
    echo "usage: $0 <image> [port]" >&2
    exit 2
fi
image=$1
port=${2:-55433}

name="crabgresql-smoke-$$"
volume="crabgresql-smoke-vol-$$"

cleanup() {
    docker rm -f "$name" > /dev/null 2>&1 || true
    docker volume rm -f "$volume" > /dev/null 2>&1 || true
}
trap cleanup EXIT

# Debian and Ubuntu keep psql off PATH in some images; the runner has it, but
# say so plainly rather than failing inside a loop.
if ! command -v psql > /dev/null; then
    echo "$0: no psql on PATH (Debian/Ubuntu: apt-get install postgresql-client)" >&2
    exit 2
fi
export PGHOST=127.0.0.1 PGPORT=$port PGUSER=postgres PGDATABASE=postgres

start() {
    docker run -d --name "$name" -p "$port:5433" -v "$volume:/var/lib/crabgresql" \
        "$image" > /dev/null
    for _ in $(seq 60); do
        if psql -qtAc 'SELECT 1' > /dev/null 2>&1; then
            return
        fi
        if [ "$(docker inspect -f '{{.State.Running}}' "$name")" != "true" ]; then
            echo "$0: the container exited during startup:" >&2
            docker logs "$name" >&2
            exit 1
        fi
        sleep 1
    done
    echo "$0: the server did not accept a connection within 60s:" >&2
    docker logs "$name" >&2
    exit 1
}

echo "== the image does not run as root"
uid=$(docker run --rm --entrypoint id "$image" -u)
if [ "$uid" = "0" ]; then
    echo "$0: the image runs as root" >&2
    exit 1
fi
echo "uid $uid"

echo "== first boot on an empty volume"
start
psql -qc 'CREATE TABLE smoke (id integer primary key, name text)'
psql -qc "INSERT INTO smoke VALUES (1, 'ferris'), (2, 'hermit')"
got=$(psql -qtAc 'SELECT name FROM smoke WHERE id = 2')
if [ "$got" != "hermit" ]; then
    echo "$0: expected 'hermit', got '$got'" >&2
    exit 1
fi

echo "== docker stop is a clean shutdown"
# A shell-form ENTRYPOINT would swallow SIGTERM and leave docker to SIGKILL
# after the timeout, so the stop is timed as well as logged: ten seconds is
# well under the default grace period and far above a real flush.
start_s=$SECONDS
docker stop -t 30 "$name" > /dev/null
elapsed=$((SECONDS - start_s))
if [ "$elapsed" -ge 10 ]; then
    echo "$0: docker stop took ${elapsed}s — PID 1 is probably not seeing SIGTERM" >&2
    docker logs "$name" >&2
    exit 1
fi
if ! docker logs "$name" 2>&1 | grep -q 'received shutdown signal'; then
    echo "$0: the server never logged a shutdown signal:" >&2
    docker logs "$name" >&2
    exit 1
fi
docker rm -f "$name" > /dev/null

echo "== the volume survives the container"
start
got=$(psql -qtAc 'SELECT name FROM smoke WHERE id = 1')
if [ "$got" != "ferris" ]; then
    echo "$0: the row did not survive the restart; got '$got'" >&2
    docker logs "$name" >&2
    exit 1
fi

echo "$0: ok ($image)"
