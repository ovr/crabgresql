-- INFORMATION_SCHEMA DOMAINS
-- The five domains initdb creates in information_schema, the array type each
-- one carries, and the two CHECK constraints two of them have. Their OIDs are
-- initdb's own, so they are pinned here against a real 18.4.
SELECT t.oid, t.typname, t.typtype, t.typbasetype::regtype AS base, t.typtypmod,
       t.typnotnull, t.typcollation, t.typdefault, t.typarray, t.typelem
  FROM pg_type t
  JOIN pg_namespace n ON n.oid = t.typnamespace
 WHERE n.nspname = 'information_schema'
   AND t.typtype = 'd'
 ORDER BY t.oid;
-- A domain borrows its physical shape and its output side from the base type,
-- and reads values in through domain_in, where the constraints run.
SELECT t.typname, t.typlen, t.typbyval, t.typcategory, t.typalign, t.typstorage,
       t.typinput, t.typoutput, t.typreceive, t.typsend
  FROM pg_type t
  JOIN pg_namespace n ON n.oid = t.typnamespace
 WHERE n.nspname = 'information_schema'
   AND t.typtype = 'd'
 ORDER BY t.oid;
-- The array types: ordinary base rows pointing back at their element. An array
-- aligns like its element but never looser than int, so _sql_identifier over
-- char-aligned `name` is still `i` while _time_stamp keeps `d`.
SELECT t.oid, t.typname, t.typtype, t.typcategory, t.typlen, t.typalign,
       t.typstorage, t.typcollation, t.typelem::regtype AS elem, t.typarray
  FROM pg_type t
  JOIN pg_type e ON e.oid = t.typelem AND e.typtype = 'd'
  JOIN pg_namespace n ON n.oid = t.typnamespace
 WHERE n.nspname = 'information_schema'
 ORDER BY t.oid;
-- Both CHECKs constrain a type rather than a relation: conrelid is 0 and
-- contypid names the domain.
SELECT c.oid, c.conname, c.contype, c.conrelid, c.contypid::regtype AS domain,
       c.convalidated, c.conkey, pg_get_constraintdef(c.oid)
  FROM pg_constraint c
  JOIN pg_namespace n ON n.oid = c.connamespace
 WHERE n.nspname = 'information_schema'
 ORDER BY c.oid;
-- Each domain belongs to its schema; its array type is internal to it; its
-- CHECK is auto-dependent on it. No edge to the base type -- int4, varchar,
-- name and timestamptz are all pinned.
-- Named from pg_type rather than through ::regtype: an array over a
-- non-built-in element is one rendering crabgresql does not do yet, and the
-- edges are what this pins.
SELECT d.classid::regclass AS class, t.typname AS obj,
       d.refclassid::regclass AS refclass, d.refobjid, d.deptype
  FROM pg_depend d
  JOIN pg_type t ON t.oid = d.objid
 WHERE d.classid = 'pg_type'::regclass
   AND d.objid IN (13712, 13713, 13715, 13716, 13717, 13718, 13723, 13724, 13725, 13726)
 ORDER BY d.objid;
SELECT d.classid::regclass AS class, d.objid, d.refclassid::regclass AS refclass,
       d.refobjid::regtype AS refobj, d.deptype
  FROM pg_depend d
 WHERE d.classid = 'pg_constraint'::regclass
   AND d.objid IN (13714, 13727)
 ORDER BY d.objid;
-- They show up in information_schema.domains like any other domain.
SELECT domain_schema, domain_name, data_type, character_maximum_length,
       datetime_precision, domain_default, udt_schema, udt_name
  FROM information_schema.domains
 WHERE domain_schema = 'information_schema'
 ORDER BY domain_name;
-- information_schema is not on the search path, so every rendering of one of
-- these names qualifies it.
SELECT 'information_schema.yes_or_no'::regtype AS by_name,
       13718::regtype AS by_oid,
       format_type(13713, NULL) AS formatted,
       pg_typeof('YES'::information_schema.yes_or_no) AS typeof;
-- The constraints really run, and the 23514 names the type the qualified way.
SELECT 'NO'::information_schema.yes_or_no;
SELECT 'MAYBE'::information_schema.yes_or_no;
SELECT 5::information_schema.cardinal_number;
SELECT (-1)::information_schema.cardinal_number;
-- The modifier lives on the domain, and an explicit cast applies it before the
-- CHECK runs: yes_or_no is varchar(3), so 'YESSIR' truncates to 'YES' and
-- passes. character_data takes no modifier and keeps the whole string.
SELECT 'YESSIR'::information_schema.yes_or_no AS truncated,
       'YESSIR'::information_schema.character_data AS unbounded;
-- The views are typed in these domains, not in their base types.
SELECT pg_typeof(schema_name) AS schema_name, pg_typeof(sql_path) AS sql_path
  FROM information_schema.schemata
 WHERE schema_name = 'public';
CREATE TABLE isd_demo (a int4, b varchar(10));
SELECT pg_typeof(table_name) AS table_name, pg_typeof(ordinal_position) AS ordinal_position,
       pg_typeof(is_nullable) AS is_nullable, pg_typeof(data_type) AS data_type
  FROM information_schema.columns
 WHERE table_name = 'isd_demo' AND ordinal_position = 1;
SELECT pg_typeof(table_type) AS table_type, pg_typeof(is_insertable_into) AS is_insertable_into
  FROM information_schema.tables
 WHERE table_name = 'isd_demo';
-- A column of a domain reports the base type's shape and names the domain
-- through the domain_* triple -- so a view over one of these columns does too.
CREATE VIEW isd_view AS SELECT is_nullable, table_name FROM information_schema.columns;
SELECT column_name, data_type, character_maximum_length,
       domain_schema, domain_name, udt_schema, udt_name
  FROM information_schema.columns
 WHERE table_name = 'isd_view'
 ORDER BY ordinal_position;
DROP VIEW isd_view;
DROP TABLE isd_demo;
