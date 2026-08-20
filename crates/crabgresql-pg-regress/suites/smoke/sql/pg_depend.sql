--
-- pg_depend: the dependency graph between database objects.
--
-- Nothing here pins a raw OID. Relation, constraint, default and rule OIDs are
-- assigned per snapshot in this build (and per cluster in PostgreSQL), so every
-- query below resolves both ends of an edge back to a name and compares the
-- graph rather than the numbering. Generated with psql -q -a against
-- PostgreSQL 18.4, then re-run here.
--
CREATE TYPE dep_mood AS ENUM ('sad', 'ok');
CREATE TABLE dep_t (
  id serial PRIMARY KEY,
  x text DEFAULT 'q',
  m dep_mood,
  y int CHECK (y > 0),
  note text
);
CREATE INDEX dep_t_x_idx ON dep_t (x);
CREATE VIEW dep_v AS SELECT id, x FROM dep_t;
CREATE SEQUENCE dep_s;
CREATE TABLE dep_plain (a int DEFAULT nextval('dep_s'));
CREATE TABLE dep_child () INHERITS (dep_t);
-- Wide enough to need out-of-line storage, which is what gives dep_t a TOAST
-- relation here: PostgreSQL creates one with any table that has a varlena
-- column, this build creates one only once a row needs it.
INSERT INTO dep_t (y, note) VALUES (1, repeat('a', 4000));
-- Every edge whose dependent is a relation, named at both ends. The sequence
-- depends on the column that owns it, the inheritance child on its parent
-- (`n`, not `a`: dropping the parent does not drop the child), and the index on
-- the column it keys.
--
-- TOAST relations are excluded here and counted below instead: PostgreSQL names
-- one after its table's OID, so its name is not stable enough to compare.
SELECT c.relname AS dependent, c.relkind,
       ref.relname AS refrelation, d.refobjsubid, d.deptype
  FROM pg_depend d
  JOIN pg_class c ON c.oid = d.objid AND d.classid = 'pg_class'::regclass
  JOIN pg_class ref ON ref.oid = d.refobjid AND d.refclassid = 'pg_class'::regclass
 WHERE (c.relname LIKE 'dep\_%' OR ref.relname LIKE 'dep\_%') AND c.relkind <> 't'
 ORDER BY 1, 3, 4, 5;
-- The TOAST relation of a table depends on it internally, named by the table
-- rather than by itself for the reason above.
--
-- PostgreSQL reports dep_child here too: it creates a TOAST relation with the
-- table, while this build creates one lazily, and nothing was ever written to
-- dep_child.
SELECT ref.relname AS toasted, d.deptype
  FROM pg_depend d
  JOIN pg_class c ON c.oid = d.objid AND d.classid = 'pg_class'::regclass
  JOIN pg_class ref ON ref.oid = d.refobjid AND d.refclassid = 'pg_class'::regclass
 WHERE ref.relname LIKE 'dep\_%' AND c.relkind = 't'
 ORDER BY 1;
-- The index that backs the primary key depends on the constraint, while the
-- plain index depends on the column it keys.
SELECT c.relname AS index, con.conname AS constraint, d.deptype
  FROM pg_depend d
  JOIN pg_class c ON c.oid = d.objid AND d.classid = 'pg_class'::regclass
  JOIN pg_constraint con ON con.oid = d.refobjid
   AND d.refclassid = 'pg_constraint'::regclass
 WHERE c.relname LIKE 'dep\_%'
 ORDER BY 1, 2;
-- Constraints onto the columns they constrain. A CHECK contributes two rows per
-- column it reads: `a` because the constraint belongs to the column, `n`
-- because its expression names it.
--
-- PostgreSQL also lists a `dep_t_id_not_null` row per table: it creates a
-- not-null constraint for every PRIMARY KEY column, which this build does not
-- (see the TODO in `SystemCatalog::constraint_oids`). The gap is in
-- pg_constraint, not here — every constraint that exists has its edges.
SELECT con.conname, con.contype, ref.relname, d.refobjsubid, d.deptype
  FROM pg_depend d
  JOIN pg_constraint con ON con.oid = d.objid
   AND d.classid = 'pg_constraint'::regclass
  JOIN pg_class ref ON ref.oid = d.refobjid AND d.refclassid = 'pg_class'::regclass
 WHERE ref.relname LIKE 'dep\_%'
 ORDER BY 1, 3, 4, 5;
-- Column defaults. A `serial` default depends on its sequence exactly as a
-- hand-written `DEFAULT nextval(...)` one does; what tells the two apart is the
-- sequence's own auto dependency in the first query above.
SELECT ref.relname, d.refobjsubid, d.deptype
  FROM pg_depend d
  JOIN pg_attrdef ad ON ad.oid = d.objid AND d.classid = 'pg_attrdef'::regclass
  JOIN pg_class ref ON ref.oid = d.refobjid AND d.refclassid = 'pg_class'::regclass
 WHERE ref.relname LIKE 'dep\_%'
 ORDER BY 1, 2, 3;
-- A view's `_RETURN` rule: internally on the view, by column on what it reads.
SELECT r.rulename, v.relname AS view, ref.relname AS reads, d.refobjsubid, d.deptype
  FROM pg_depend d
  JOIN pg_rewrite r ON r.oid = d.objid AND d.classid = 'pg_rewrite'::regclass
  JOIN pg_class v ON v.oid = r.ev_class
  JOIN pg_class ref ON ref.oid = d.refobjid AND d.refclassid = 'pg_class'::regclass
 WHERE v.relname LIKE 'dep\_%'
 ORDER BY 2, 3, 4;
-- A column of a user-defined type depends on that type. A column of a built-in
-- one does not: PostgreSQL records no dependency on a pinned object, which is
-- why only `m` appears here.
SELECT c.relname, a.attname, t.typname, d.deptype
  FROM pg_depend d
  JOIN pg_class c ON c.oid = d.objid AND d.classid = 'pg_class'::regclass
  JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = d.objsubid
  JOIN pg_type t ON t.oid = d.refobjid AND d.refclassid = 'pg_type'::regclass
 WHERE c.relname LIKE 'dep\_%'
 ORDER BY 1, 2;
-- Relations and types belong to their schema. An index and a TOAST relation do
-- not carry this edge.
SELECT c.relname, c.relkind, n.nspname, d.deptype
  FROM pg_depend d
  JOIN pg_class c ON c.oid = d.objid AND d.classid = 'pg_class'::regclass
  JOIN pg_namespace n ON n.oid = d.refobjid
   AND d.refclassid = 'pg_namespace'::regclass
 WHERE c.relname LIKE 'dep\_%'
 ORDER BY 1;
-- pg_get_serial_sequence answers from the same auto edge: the `serial` column
-- owns a sequence, a hand-written nextval default does not, and neither does an
-- ordinary column.
SELECT pg_get_serial_sequence('dep_t', 'id') AS serial,
       pg_get_serial_sequence('dep_plain', 'a') AS plain_default,
       pg_get_serial_sequence('dep_t', 'x') AS no_sequence;
SELECT pg_get_serial_sequence('public.dep_t', 'id') AS qualified;
SELECT pg_get_serial_sequence('dep_nosuch', 'id');
SELECT pg_get_serial_sequence('dep_t', 'nope');
-- A system catalog answers too: nothing there owns a sequence, but its columns
-- exist, so a real column is NULL and a made-up one is still an error.
SELECT pg_get_serial_sequence('pg_class', 'relname') AS catalog_column;
SELECT pg_get_serial_sequence('pg_class', 'nosuchcol');
-- A quoted, mixed-case table keeps the link: the `nextval` text the default
-- stores is quoted the way PostgreSQL quotes it, so reading it back finds the
-- sequence rather than a lower-cased name that does not exist.
CREATE TABLE "DepMix" ("Id" serial);
SELECT pg_get_serial_sequence('"DepMix"', 'Id') AS mixed_case;
SELECT c.relname AS sequence, ref.relname AS owner, d.refobjsubid, d.deptype
  FROM pg_depend d
  JOIN pg_class c ON c.oid = d.objid AND d.classid = 'pg_class'::regclass
  JOIN pg_class ref ON ref.oid = d.refobjid AND d.refclassid = 'pg_class'::regclass
 WHERE c.relname = 'DepMix_Id_seq'
 ORDER BY 1, 2;
DROP TABLE "DepMix";
-- A relation a view reads only from an expression subquery is a dependency all
-- the same: dropping it would break the view, and a dump that restored it after
-- the view would fail.
--
-- Two divergences here, both coarser than PostgreSQL and both deliberate: the
-- subquery's own relation is recorded whole (PostgreSQL names the column), and
-- the outer relation is recorded column by column but widened to all of them,
-- because an expression subquery stops the projection pass from proving which
-- ones the view reads. Coarser costs a refused DROP; narrower would cost a
-- dependency nobody sees.
CREATE TABLE dep_sub (b int);
CREATE VIEW dep_subv AS SELECT id, (SELECT max(b) FROM dep_sub) AS m FROM dep_t;
SELECT ref.relname AS reads, d.refobjsubid, d.deptype
  FROM pg_depend d
  JOIN pg_rewrite r ON r.oid = d.objid AND d.classid = 'pg_rewrite'::regclass
  JOIN pg_class v ON v.oid = r.ev_class
  JOIN pg_class ref ON ref.oid = d.refobjid AND d.refclassid = 'pg_class'::regclass
 WHERE v.relname = 'dep_subv'
 ORDER BY 1, 2;
DROP VIEW dep_subv;
DROP TABLE dep_sub;
-- `OWNED BY NONE` names no owner, which is PostgreSQL's default spelled out.
CREATE SEQUENCE dep_none OWNED BY NONE;
DROP SEQUENCE dep_none;
DROP VIEW dep_v;
DROP TABLE dep_child;
DROP TABLE dep_plain;
DROP TABLE dep_t;
DROP SEQUENCE dep_s;
DROP TYPE dep_mood;
