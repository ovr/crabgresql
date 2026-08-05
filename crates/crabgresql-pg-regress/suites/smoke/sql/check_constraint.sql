-- CHECK CONSTRAINTS
-- Column-level, table-level, named and generated names.
CREATE TABLE chk_names (
  x integer CHECK (x > 3),
  y integer,
  CONSTRAINT chk_named CHECK (x + y < 100),
  CHECK (y <> 0)
);
-- Two column checks on one column dedupe with a numeric suffix.
CREATE TABLE chk_dedup (c integer CHECK (c > 0) CHECK (c < 10));
SELECT conname, contype, conkey, convalidated, conislocal, coninhcount, connoinherit,
       pg_get_expr(conbin, conrelid) AS conbin,
       pg_get_constraintdef(oid) AS def
  FROM pg_constraint
 WHERE conrelid IN ('chk_names'::regclass, 'chk_dedup'::regclass)
 ORDER BY conname;
SELECT relname, relchecks FROM pg_class
 WHERE relname IN ('chk_names', 'chk_dedup') ORDER BY relname;
-- An OID no constraint answers to is NULL, not an error.
SELECT pg_get_constraintdef(999999) IS NULL AS unknown_oid_is_null;

-- Enforcement on INSERT: each constraint reports itself.
INSERT INTO chk_names VALUES (5, 5);
INSERT INTO chk_names VALUES (1, 5);
INSERT INTO chk_names VALUES (5, 0);
INSERT INTO chk_names VALUES (50, 60);
-- A predicate that evaluates to NULL PASSES: only false is a violation.
INSERT INTO chk_names VALUES (NULL, 5);
SELECT x, y FROM chk_names ORDER BY x NULLS LAST;
-- UPDATE re-checks every constraint against the new row.
UPDATE chk_names SET x = 1 WHERE x = 5;

-- Ordering: NOT NULL is reported before CHECK, CHECK before UNIQUE.
CREATE TABLE chk_order (a integer NOT NULL, b integer UNIQUE, c integer CHECK (c > 0));
INSERT INTO chk_order VALUES (1, 1, 1);
INSERT INTO chk_order VALUES (NULL, 2, -1);
INSERT INTO chk_order VALUES (2, 1, -1);
-- Two violated checks resolve to the alphabetically first name.
CREATE TABLE chk_two (a integer, CONSTRAINT zzz CHECK (a > 100), CONSTRAINT aaa CHECK (a > 200));
INSERT INTO chk_two VALUES (0);

-- DDL errors.
CREATE TABLE chk_bad (a integer CHECK (nosuchcol > 0));
CREATE TABLE chk_bad (a integer CHECK (1));
-- A subquery in a CHECK is 0A000, but it is exercised in `e2e.rs` rather than
-- here: PostgreSQL puts the caret on the subquery's opening paren, and this
-- parser's span for a parenthesised subquery starts at SELECT instead, so the
-- cursor line cannot match byte-for-byte. The SQLSTATE and message do.
CREATE TABLE chk_bad (a integer, CHECK (count(*) > 0));
CREATE TABLE chk_bad (a integer, CONSTRAINT dup CHECK (a > 0), CONSTRAINT dup CHECK (a < 9));

-- ALTER TABLE ... ADD CONSTRAINT ... CHECK.
CREATE TABLE chk_alter (a integer);
INSERT INTO chk_alter VALUES (1), (5);
ALTER TABLE chk_alter ADD CONSTRAINT chk_alter_c CHECK (a > 3);
ALTER TABLE chk_alter ADD CONSTRAINT chk_alter_c CHECK (a > 0);
INSERT INTO chk_alter VALUES (-1);
ALTER TABLE chk_alter ADD CONSTRAINT chk_alter_c CHECK (a < 9);
ALTER TABLE chk_alter ADD CHECK (a < 1000);
SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint
 WHERE conrelid = 'chk_alter'::regclass ORDER BY conname;

-- Inheritance: a parent's checks are copied into every child.
CREATE TABLE chk_parent (a integer, CONSTRAINT chk_p CHECK (a > 0));
CREATE TABLE chk_child () INHERITS (chk_parent);
SELECT conrelid::regclass::text AS rel, conname, conislocal, coninhcount
  FROM pg_constraint WHERE conname = 'chk_p' ORDER BY rel;
INSERT INTO chk_child VALUES (-1);
-- ADD CHECK on the parent recurses to the child.
ALTER TABLE chk_parent ADD CONSTRAINT chk_p2 CHECK (a < 100);
SELECT conrelid::regclass::text AS rel, conname, conislocal, coninhcount
  FROM pg_constraint WHERE conname = 'chk_p2' ORDER BY rel;
INSERT INTO chk_child VALUES (500);
-- A child redeclaring the same predicate merges; a different one collides.
CREATE TABLE chk_merge (CONSTRAINT chk_p CHECK (a > 0)) INHERITS (chk_parent);
SELECT conname, conislocal, coninhcount FROM pg_constraint
 WHERE conrelid = 'chk_merge'::regclass ORDER BY conname;
CREATE TABLE chk_conflict (CONSTRAINT chk_p CHECK (a > 5)) INHERITS (chk_parent);
-- Two parents contributing one name with different predicates.
CREATE TABLE chk_other (a integer, CONSTRAINT chk_p CHECK (a > 9));
CREATE TABLE chk_two_parents () INHERITS (chk_parent, chk_other);

-- The suite shares one database, and the inheritance links above are visible to
-- every later test that reads pg_inherits — so this one cleans up after itself.
DROP TABLE chk_merge;
DROP TABLE chk_child;
DROP TABLE chk_parent;
DROP TABLE chk_other;
