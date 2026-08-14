---
name: verify
description: Drive the crabgresql server end-to-end over a real pgwire socket to observe a change working. Use when verifying engine, WAL, recovery, planner, or SQL-surface changes in this repo.
---

# Verifying crabgresql

The surface is a **pgwire socket**: build the server binary, run it on a throwaway
`pgdata`, and drive it with the local `psql`. Unit tests are not verification here —
durability changes in particular only show up across process restarts.

## Handle

```bash
cargo build -p crabgresql-server --bin crabgresql        # ~10s incremental
D=$(mktemp -d /tmp/cgv.XXXXXX)
target/debug/crabgresql -D "$D" -p 55999 > /tmp/srv.log 2>&1 &
sleep 3                                                  # recovery + listener
psql -h 127.0.0.1 -p 55999 -q -c "SELECT 1;"
```

- Port defaults to 5433 (one above PG's) — pass `-p` to avoid colliding with
  another workspace's server.
- `-D`/`--data-dir` defaults to `./pgdata`; always pass a temp dir.
- **Each `Bash` call is a fresh shell.** Shell variables do not persist between
  calls — write `$D` to a file (`echo "D=$D" > /tmp/cgv.env`; `source` it later) or
  re-derive it with `ls -d /tmp/cgv.*`.
- Local `psql` is PG 18; the server advertises 19.0, so `psql` shapes some
  introspection queries accordingly (see the `psql \d` notes in the repo).

## Lifecycle, which is what durability work needs

| Action | Command | What it does |
|---|---|---|
| clean shutdown | `pkill -TERM -f "crabgresql -D $D"` | checkpoints, sets `clean_shutdown=1`, publishes a redo point |
| crash | `pkill -KILL -f "crabgresql -D $D"` | no checkpoint; next start replays from the last published redo point |
| restart | rerun the launch line | runs recovery; `/tmp/srv.log` shows `running recovery` |

Unlogged tables' data survives only a *clean* shutdown — a crash, or a control file
that reads as absent, resets them. That is a useful signal, not a bug.

## Reading `global/pg_control`

32-byte v2 image, little-endian: `magic u32 (0xCA6D0001) | version u32 (2) |
next_xid u64 | redo_lsn u64 | clean_shutdown u8 | 3 pad | crc32c u32 over [0..28)`.
Version 1 is the 24-byte predecessor with no `redo_lsn` and the CRC at 20.

```bash
python3 -c "
import struct; b=open('$D/global/pg_control','rb').read()
print('version', struct.unpack_from('<I',b,4)[0],
      'next_xid', struct.unpack_from('<Q',b,8)[0],
      'redo_lsn', struct.unpack_from('<Q',b,16)[0],
      'clean', b[24])"
```

The CRC is **CRC-32C (Castagnoli, poly 0x82F63B78)**, not zlib's CRC-32 — hand-writing
a control file needs the right one or it reads as absent.

## Proving a bounded replay is real

Rows already flushed to disk survive even a replay that reads nothing, so "the rows
are still there" alone proves little. Two checks that cannot pass by accident:

1. **Scribble the prefix.** Overwrite `[0, redo_lsn)` of the stream with garbage,
   then restart. If recovery read it, the first record fails to decode, the log reads
   as empty, and `reset_to` truncates the stream — so also assert the stream is
   *longer* than `redo_lsn` afterwards.
2. **Commit above the redo point, then crash.** Those rows exist only in the WAL
   suffix, so they come back only if replay actually ran from the redo point.

An LSN is a position in the *stream*, and the stream is cut into 32 MiB segment
files named `pg_wal/<24 hex digits of the segment number>` — so byte `l` lives in
segment `l // (32<<20)` at offset `l % (32<<20)`. Anything that pokes at the log by
LSN has to do that arithmetic; writing at offset `redo_lsn` of the first segment
would stop at the first boundary and, worse, grow a file whose size is fixed.

```bash
python3 -c "
import os
SEG = 32 << 20; D = '$D'; REDO = REDO
for seg in range(REDO // SEG + 1):
    f = os.open('%s/pg_wal/%024X' % (D, seg), os.O_RDWR)
    os.pwrite(f, b'\xAB' * min(REDO - seg * SEG, SEG), 0); os.fsync(f); os.close(f)"
```

The stream's on-disk end is `last_segment * 32MiB + len(last_segment)`, not the size
of any one file.

## Gotchas

- A relation with rows in a RAM write buffer (`USING buffer`, or a Parquet write
  buffer) makes a checkpoint publish `redo_lsn = 0` on purpose — those rows live only
  in the WAL. Use a plain heap table when you want a bounded redo point.
- `cargo test --workspace` leaves `crabgresql-server`'s `e2e` binary aborting on
  `recursive_view_definition_errors_on_use_not_creation` (stack overflow). Pre-existing
  and unrelated; `-- --skip recursive_view_definition` to get a clean run.
- Startup errors print as a raw `Debug` of `std::io::Error` (`Custom { kind: Other,
  error: Redo("...") }`) because `main` returns `io::Result`. The message inside is
  the real one.
- Do not run `cargo fmt --all` — the tree is not clean under rustfmt 1.8.0; format by
  hand.
