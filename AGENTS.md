# AGENTS.md

## Clean-room approach

We reproduce PostgreSQL's **behavior**, not its code. These rules are binding
for the entire codebase:

- It is **forbidden** to port PostgreSQL C code to Rust line-by-line or by
  "translation" — even where we match semantics 1:1 (visibility checks, the
  lock conflict matrix, cast rules, EvalPlanQual, SSI).
- It is **allowed** to rely on: the official PG documentation,
  architecture-level READMEs/comments (algorithm descriptions), publications
  (Lehman-Yao, ARIES, Cahill / Ports & Grittner on SSI), and the observable
  behavior of real PG via differential tests.
- Phrases in this document like "ported verbatim" mean **semantics**: the same
  decision logic, confirmed by tests — implemented independently.
- Borrowing upstream **data** is acceptable: catalog generation from
  `pg_type.dat`/`pg_proc.dat`, the regression- and isolation-test corpora. The
  PostgreSQL License (permissive, BSD-like) is compatible with Apache-2.0 —
  attribution goes into `NOTICE`.
- Error messages and EXPLAIN output match PG intentionally (they are part of
  compatibility) — short messages and output formats are not copyrightable in
  that sense, but we take them from observed behavior, not from the sources.

### Practical consequences for contributors

- Describe behavior in terms of *what PG does* (observable output, error text,
  SQLSTATE), citing the documentation, an algorithm description, or a
  differential/regression test — never a C source line or file.
- Third-party Rust libraries with compatible licenses may be vendored and
  adapted (e.g. `vendor/ryu`, a fork of the `ryu` crate configured to match
  PG's observable float output). Record attribution in `NOTICE`.
- When a comment needs to explain why a value or branch exists, point to the
  behavior it reproduces (e.g. "matches `float8out` at `extra_float_digits = 0`",
  with a regression-test citation), not to PG's implementation.

## Internal interfaces

- The project is pre-1.0 with a single in-tree consumer of every internal API,
  so **breaking changes to internal interfaces are fine** — Rust traits,
  function signatures, module boundaries, and crate-internal types. Change the
  shape that fits the new code best and update all call sites.
- **Do not** add compatibility shims, deprecated aliases, or parallel "old + new"
  method pairs just to avoid touching callers. Prefer one clean interface over a
  backward-compatible one; churn across the workspace is expected and cheap.
- This freedom stops at two boundaries that are *not* internal: **persisted /
  on-disk formats** (e.g. the relation catalog — old files must still load, via
  explicit versioned/back-compatible decoding) and **PG-facing observable
  behavior** (wire protocol, SQL surface, error text/SQLSTATE, EXPLAIN output).

## Regression test scoreboard

- When you land work that changes how many regression tests pass — promoting a
  test to `crates/crabgresql-pg-regress/suites/upstream_must_pass.txt`, or
  making more of the suite green — **update the passed-tests counter** that
  tracks compatibility progress (the score in `README.md`) in the same change.
  A stale counter misreports the project's state.
- **Count tests by their number, not by file.** The score is the count of
  individual tests that pass, not the count of `.sql` files: a partially-passing
  file contributes only the tests that actually pass, and is never rounded up to
  a whole file or dropped to zero. Report the honest per-test total.

## Comments

- **Do not restate the code.** A comment that says what the next line already
  says — `// increment the counter`, `# the build job`, a doc comment that
  echoes the function name — costs a reader time and goes stale silently. This
  applies to configuration and CI files as much as to Rust.
- Comment the things the code cannot say: why a branch or a constant exists,
  which observable PG behavior it reproduces, what breaks if it is changed, or
  which non-obvious alternative was rejected and why.
- If nothing of that kind is true of a line, leave it uncommented.

## Rust error handling

- Never use `unwrap()`.
- To assert that an operation fails, use `expect_err("…")` — `clippy::unwrap_used`
  denies `unwrap_err()` too, and CI lints test targets.
- In tests, return `Result<(), _>` and propagate errors with `?` instead of
  calling `unwrap()` for successful fallible operations.
