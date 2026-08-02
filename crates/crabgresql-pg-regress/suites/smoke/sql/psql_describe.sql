--
-- PSQL_DESCRIBE
-- The catalog surface real psql's `\d` family reads: pg_am, pg_get_userbyid,
-- and pg_table_is_visible. The runner cannot execute backslash metacommands, so
-- the queries psql sends are pasted literally.
--
-- Three adaptations keep the output identical to PostgreSQL's: relations are
-- filtered to this file's own `dtest%` names (the whole suite shares one
-- database, so an unfiltered listing would depend on schedule order); the owner
-- is only tested for non-NULL, because the reference server's role is the OS
-- user while the regress client connects as `postgres`; and the temp relation
-- is created after the listings, since PostgreSQL numbers `pg_temp_N` by
-- backend slot and the name would not reproduce.
--
-- pg_am lists PostgreSQL's built-in access methods plus crabgresql's own
SELECT oid, amname, amhandler, amtype FROM pg_catalog.pg_am ORDER BY oid;
-- pg_get_userbyid never returns NULL: an unowned OID prints a placeholder, and
-- a negative argument reinterprets as unsigned rather than clamping
SELECT pg_catalog.pg_get_userbyid(999999), pg_catalog.pg_get_userbyid(-1);
-- both functions are STRICT
SELECT pg_catalog.pg_get_userbyid(NULL) IS NULL AS role_null,
       pg_catalog.pg_table_is_visible(NULL) IS NULL AS visible_null;
-- an OID no relation has is NULL, not false
SELECT pg_catalog.pg_table_is_visible(999999) IS NULL AS unknown_is_null;
CREATE TABLE dtest_tbl (id int PRIMARY KEY, label text);
CREATE VIEW dtest_view AS SELECT id FROM dtest_tbl;
CREATE SEQUENCE dtest_seq;
CREATE SCHEMA dtest_schema;
CREATE TABLE dtest_schema.dtest_hidden (x int);
-- a table's relam joins to pg_am; a view has no access method
SELECT c.relname, a.amname
  FROM pg_catalog.pg_class c
  LEFT JOIN pg_catalog.pg_am a ON a.oid = c.relam
 WHERE c.relname IN ('dtest_tbl', 'dtest_view')
 ORDER BY 1;
-- visibility follows unqualified name resolution: a relation in a named schema
-- is reachable only when qualified, so it is not visible
SELECT c.relname, pg_catalog.pg_table_is_visible(c.oid) AS visible
  FROM pg_catalog.pg_class c
 WHERE c.relname IN ('dtest_tbl', 'dtest_hidden')
 ORDER BY 1;
-- the query psql's `\d` sends, verbatim but for the owner projection and the
-- name filter. Exercises the three-way LEFT JOIN, the simple CASE over a text
-- column with no ELSE, the `!~` regex on a name column, and both functions.
SELECT n.nspname as "Schema",
  c.relname as "Name",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned index' END as "Type",
  pg_catalog.pg_get_userbyid(c.relowner) IS NOT NULL as "Owner?"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
WHERE c.relkind IN ('r','p','v','m','S','f','')
      AND n.nspname <> 'pg_catalog'
      AND n.nspname !~ '^pg_toast'
      AND n.nspname <> 'information_schema'
  AND c.relname LIKE 'dtest%'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;
-- the same shape psql's `\di` sends: the index rows carry their access method
SELECT c.relname as "Name",
  CASE c.relkind WHEN 'i' THEN 'index' WHEN 'I' THEN 'partitioned index' END as "Type",
  am.amname as "Method",
  c2.relname as "Table"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
     LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid
     LEFT JOIN pg_catalog.pg_class c2 ON i.indrelid = c2.oid
WHERE c.relkind IN ('i','I','')
      AND c.relname LIKE 'dtest%'
      AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1;
-- a temp relation belongs to this session, so it is visible
CREATE TEMP TABLE dtest_temp (y int);
SELECT pg_catalog.pg_table_is_visible(c.oid) AS visible
  FROM pg_catalog.pg_class c
 WHERE c.relname = 'dtest_temp';
DROP TABLE dtest_temp;
DROP TABLE dtest_schema.dtest_hidden;
DROP SCHEMA dtest_schema;
DROP VIEW dtest_view;
DROP SEQUENCE dtest_seq;
DROP TABLE dtest_tbl;
--
-- What `\d <table>` and `\d <view>` read past that listing: `format_type`,
-- `pg_get_expr`, and the `pg_class`/`pg_attribute` columns. Type modifiers are
-- written as literal (OID, atttypmod) pairs so these cases do not depend on the
-- `reg*` casts.
--
-- format_type spells a type as `\d` shows it. numeric packs (precision, scale)
-- above the four-byte varlena header the character types also reserve; the bit
-- types store their length directly.
SELECT pg_catalog.format_type(1700, 262150) AS "numeric",
       pg_catalog.format_type(1043, 24) AS "varchar",
       pg_catalog.format_type(1042, 14) AS "bpchar",
       pg_catalog.format_type(1560, 5) AS "bit";
-- a datetime precision prints before the time-zone suffix, not after it; an
-- array formats its element type and appends `[]`
SELECT pg_catalog.format_type(1114, 3) AS "timestamp",
       pg_catalog.format_type(1184, 3) AS "timestamptz",
       pg_catalog.format_type(1007, NULL) AS "int4_array";
-- format_type is strict in its OID but not in its modifier: a NULL modifier
-- means "no modifier" (psql's sequence query relies on it). OID 0 prints `-`
-- and an OID no type has prints `???`, neither of them NULL.
SELECT pg_catalog.format_type(1043, NULL) AS unmodified,
       pg_catalog.format_type(NULL, 24) IS NULL AS null_type,
       pg_catalog.format_type(0, NULL) AS zero,
       pg_catalog.format_type(999999, NULL) AS unknown;
-- a modifier below its type's threshold prints nothing rather than a negative
-- length: the character types need more than the four-byte header they
-- reserve, numeric needs at least it
SELECT pg_catalog.format_type(1043, 2) AS vc_below,
       pg_catalog.format_type(1043, 4) AS vc_at,
       pg_catalog.format_type(1043, 5) AS vc_above,
       pg_catalog.format_type(1700, 3) AS num_below,
       pg_catalog.format_type(1700, 4) AS num_at;
-- numeric's scale is a *signed* 11-bit field, so a negative scale round trips
SELECT pg_catalog.format_type(1700, 264194) AS neg_scale,
       pg_catalog.format_type(1700, 2147483647) AS max_modifier;
-- `bpchar` is the one type that distinguishes "a modifier was given, but it is
-- the no-modifier value" from "no modifier at all"
SELECT pg_catalog.format_type(1042, -1) AS given_none,
       pg_catalog.format_type(1042, NULL) AS not_given;
CREATE TABLE dfmt_t (id int PRIMARY KEY, code varchar(20), tag char(4), mask bit(5), note text);
-- pg_attribute reports atttypmod in PostgreSQL's encoding, so format_type over
-- it reproduces `\d`'s Type column. No column is an identity or generated one,
-- which PostgreSQL spells as the empty string rather than NULL.
SELECT a.attname, a.atttypid, a.atttypmod,
       pg_catalog.format_type(a.atttypid, a.atttypmod) AS "Type",
       a.attidentity, a.attgenerated
  FROM pg_catalog.pg_attribute a, pg_catalog.pg_class c
 WHERE c.relname = 'dfmt_t' AND a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
 ORDER BY a.attnum;
CREATE TABLE dfmt_d (id int DEFAULT 42, label text);
-- the third query psql's `\d` sends, verbatim except that the relation is found
-- by name instead of by the OID psql substitutes. The default subquery
-- exercises pg_get_expr; the collation subquery self-filters to NULL because a
-- column with no explicit COLLATE carries its type's own collation.
SELECT a.attname,
  pg_catalog.format_type(a.atttypid, a.atttypmod),
  (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true)
   FROM pg_catalog.pg_attrdef d
   WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef),
  a.attnotnull,
  (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t
   WHERE c.oid = a.attcollation AND t.oid = a.atttypid AND a.attcollation <> t.typcollation) AS attcollation,
  a.attidentity,
  a.attgenerated
FROM pg_catalog.pg_attribute a, pg_catalog.pg_class c
WHERE c.relname = 'dfmt_d' AND a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum;
CREATE VIEW dfmt_v AS SELECT id FROM dfmt_d;
-- the pg_class columns psql's second `\d` query projects. Nothing here has a
-- CHECK constraint, trigger, row security, typed-table parent, or non-default
-- tablespace; only a view owns a rule; and a heap-backed relation defaults its
-- replica identity to the primary key while a view has none. (`reltoastrelid`
-- is deliberately not projected: PostgreSQL gives a text column's table a real
-- TOAST relation, which crabgresql does not model.)
SELECT c.relname, c.relkind, c.relchecks, c.relhasrules, c.relhastriggers,
       c.relrowsecurity, c.relforcerowsecurity, c.relpersistence, c.relreplident,
       c.reloftype, c.reltablespace, c.relispartition
  FROM pg_catalog.pg_class c
 WHERE c.relname IN ('dfmt_d', 'dfmt_v')
 ORDER BY 1;
CREATE TABLE dfmt_p (a int, b text) PARTITION BY RANGE (a);
CREATE TABLE dfmt_p1 PARTITION OF dfmt_p FOR VALUES FROM (1) TO (10);
CREATE TABLE dfmt_p2 PARTITION OF dfmt_p FOR VALUES FROM (10) TO (MAXVALUE);
-- a negative bound is quoted, where a non-negative one is bare
CREATE TABLE dfmt_p3 PARTITION OF dfmt_p FOR VALUES FROM (MINVALUE) TO (-10);
-- a leaf partition's bound, deparsed back to SQL. The partitioned parent itself
-- has no bound.
SELECT c.relname, c.relkind, c.relispartition,
       pg_catalog.pg_get_expr(c.relpartbound, c.oid) AS bound
  FROM pg_catalog.pg_class c
 WHERE c.relname LIKE 'dfmt_p%'
 ORDER BY 1;
-- a boolean bound is an SQL keyword, not the `t`/`f` of the wire encoding
CREATE TABLE dfmt_b (a bool, b int) PARTITION BY RANGE (a);
CREATE TABLE dfmt_b1 PARTITION OF dfmt_b FOR VALUES FROM (false) TO (true);
SELECT c.relname, pg_catalog.pg_get_expr(c.relpartbound, c.oid) AS bound
  FROM pg_catalog.pg_class c
 WHERE c.relname = 'dfmt_b1';
DROP TABLE dfmt_b;
DROP TABLE dfmt_p;
DROP VIEW dfmt_v;
DROP TABLE dfmt_d;
DROP TABLE dfmt_t;
-- `bit` is the one type besides `bpchar` whose spelling depends on *whether* a
-- modifier was given: with one it prints `bit(4)`, with the -1 that means "none"
-- it is quoted, and that quoted form is what a deparsed constant's label uses.
SELECT pg_catalog.format_type(1560, -1) AS "bit -1",
       pg_catalog.format_type(1560, NULL) AS "bit none",
       pg_catalog.format_type(1562, -1) AS "varbit -1";
-- A literal column default is stored already deparsed, so `\d` prints what
-- PostgreSQL prints. The type label is the *literal's* own type with no
-- modifier, so an untyped '1001' takes the column's type while B'0101' stays
-- `bit` even in a `bit varying` column; int4 and a fractional numeric print
-- bare, a negative one does not; and the value is re-rendered by the type's
-- output function ('007' -> 7, and 'x' stays unpadded in a char(4)).
-- Non-literal defaults are not rewritten and print as written, which is a known
-- divergence (PostgreSQL deparses the node), so none appears here.
CREATE TABLE dfmt_def (
  b1 bit(4) DEFAULT '1001',
  b2 bit(4) DEFAULT B'0101',
  b3 bit varying(5) DEFAULT '1001',
  b4 bit varying(5) DEFAULT B'0101',
  i1 integer DEFAULT 42,
  i2 integer DEFAULT -1,
  i3 bigint DEFAULT 42,
  n1 numeric(5,2) DEFAULT 1.5,
  n2 numeric DEFAULT -1.5,
  t1 text DEFAULT 'it''s',
  c1 char(4) DEFAULT 'x',
  bo boolean DEFAULT true,
  d1 date DEFAULT '2020-01-02',
  i4 integer DEFAULT '007',
  nn text NOT NULL,
  co text COLLATE "de-x-icu"
);
\d dfmt_def
DROP TABLE dfmt_def;
-- `DEFAULT NULL` is recorded only when the column's type needs a length
-- coercion to accept it; for everything else PostgreSQL drops the default
-- outright, leaving `atthasdef` false. `name` is the one type that looks like it
-- should be in the first group and is not.
CREATE TABLE dfmt_null (
  a text DEFAULT NULL, b bit(4) DEFAULT NULL, c varchar(4) DEFAULT NULL,
  d varchar DEFAULT NULL, e numeric(5,2) DEFAULT NULL, f numeric DEFAULT NULL,
  g char(4) DEFAULT NULL, h integer DEFAULT NULL, i timestamp(3) DEFAULT NULL,
  j bit varying(5) DEFAULT NULL, k name DEFAULT NULL
);
SELECT a.attname, a.atthasdef,
       pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) AS def
  FROM pg_catalog.pg_attribute a
       LEFT JOIN pg_catalog.pg_attrdef d
              ON d.adrelid = a.attrelid AND d.adnum = a.attnum
 WHERE a.attrelid = 'dfmt_null'::regclass AND a.attnum > 0
 ORDER BY a.attnum;
DROP TABLE dfmt_null;
-- A default that is not a literal is deparsed too, so it comes back canonical
-- rather than as typed: a function argument carries the type its signature gives
-- it (`nextval` takes a regclass, `upper` a text), and the stored form is the
-- fully parenthesised one. psql's `\d` asks for the `pretty` rendering, which
-- drops the parentheses precedence already implies.
CREATE SEQUENCE dfmt_seq;
CREATE TABLE dfmt_expr (
  a integer DEFAULT (1 + 2),
  b text DEFAULT 'a' || 'b',
  c integer DEFAULT nextval('dfmt_seq'),
  e integer DEFAULT (2 * (3 + 4)),
  f text DEFAULT upper('x'),
  h boolean DEFAULT (1 < 2)
);
SELECT a.attname,
       pg_catalog.pg_get_expr(d.adbin, d.adrelid) AS plain,
       pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) AS pretty
  FROM pg_catalog.pg_attribute a
       JOIN pg_catalog.pg_attrdef d
         ON d.adrelid = a.attrelid AND d.adnum = a.attnum
 WHERE a.attrelid = 'dfmt_expr'::regclass
 ORDER BY a.attnum;
-- the deparse is a rendering, not a rewrite: the defaults still evaluate
INSERT INTO dfmt_expr (a) VALUES (DEFAULT);
SELECT a, b, c, e, f, h FROM dfmt_expr;
DROP TABLE dfmt_expr;
DROP SEQUENCE dfmt_seq;
-- A `timestamptz` constant is the one default whose text belongs to the reader:
-- PostgreSQL stores the instant and renders it in the session's zone. A `timetz`
-- carries its own offset and a zone-less `timestamp` has none, so neither moves.
CREATE TABLE dfmt_zone (
  a timestamptz DEFAULT '2020-01-01 00:00:00+02',
  b timetz DEFAULT '12:00:00+02',
  c timestamp DEFAULT '2020-01-01 00:00:00'
);
SET TIME ZONE 'UTC';
SELECT a.attname, pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) AS def
  FROM pg_catalog.pg_attribute a
       JOIN pg_catalog.pg_attrdef d
         ON d.adrelid = a.attrelid AND d.adnum = a.attnum
 WHERE a.attrelid = 'dfmt_zone'::regclass
 ORDER BY a.attnum;
SET TIME ZONE 'Asia/Tokyo';
SELECT a.attname, pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) AS def
  FROM pg_catalog.pg_attribute a
       JOIN pg_catalog.pg_attrdef d
         ON d.adrelid = a.attrelid AND d.adnum = a.attnum
 WHERE a.attrelid = 'dfmt_zone'::regclass
 ORDER BY a.attnum;
RESET timezone;
DROP TABLE dfmt_zone;
