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
