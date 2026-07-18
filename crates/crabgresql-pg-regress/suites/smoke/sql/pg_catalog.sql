--
-- PG_CATALOG
-- Core pg_catalog relations are queryable, both schema-qualified
-- (pg_catalog.pg_type) and unqualified (pg_catalog is implicitly on the search
-- path). Queries touch only columns/values that match PostgreSQL exactly;
-- count(*) is avoided because crabgresql exposes a curated subset of the
-- built-in types. Output hand-checked against PostgreSQL's psql -a -q format.
--
-- unqualified pg_type resolves via the implicit pg_catalog search path
SELECT oid, typname, typlen, typbyval
  FROM pg_type
 WHERE typname IN ('bool', 'int4', 'text')
 ORDER BY oid;
-- driver-critical OIDs match PostgreSQL (23 = int4, 25 = text, 26 = oid)
SELECT typname FROM pg_type WHERE oid = 23;
-- schema-qualified access routes straight to pg_catalog
SELECT oid, typname, typcategory FROM pg_catalog.pg_type WHERE typname = 'oid';
-- ORDER BY on an oid column
SELECT typname
  FROM pg_type
 WHERE typname IN ('text', 'bool', 'int4')
 ORDER BY oid;
-- pg_namespace exposes the reserved schemas and public
SELECT oid, nspname
  FROM pg_namespace
 WHERE nspname IN ('pg_catalog', 'pg_toast', 'public')
 ORDER BY oid;
-- a user table and pg_catalog resolve independently on the same statement stream
CREATE TABLE pgcat_demo (a int, b text);
INSERT INTO pgcat_demo VALUES (1, 'x');
SELECT typname FROM pg_catalog.pg_type WHERE typname = 'int4';
SELECT a, b FROM pgcat_demo;
-- pg_class reflects a live user relation
CREATE TABLE pgcat_reflect (id int4, label text);
SELECT relname, relkind, relnatts, relpersistence
  FROM pg_class
 WHERE relname = 'pgcat_reflect';
-- pg_attribute joined to pg_class lists that relation's columns, in order,
-- with PG's type OIDs (23 = int4, 25 = text) and typlen (-1 = varlena)
SELECT a.attname, a.atttypid, a.attlen, a.attnum
  FROM pg_class c, pg_attribute a
 WHERE a.attrelid = c.oid AND c.relname = 'pgcat_reflect'
 ORDER BY a.attnum;
-- pg_cast: int4 -> int8 is an implicit cast, int8 -> int4 is assignment-only
SELECT castsource, casttarget, castcontext
  FROM pg_cast
 WHERE castsource IN (20, 23) AND casttarget IN (20, 23)
 ORDER BY castsource, casttarget;
-- a schema-qualified write reaches the permanent relation (INSERT accepts the
-- public. qualifier symmetrically with SELECT/UPDATE)
CREATE TABLE pgcat_pub (v int);
INSERT INTO public.pgcat_pub VALUES (7);
UPDATE public.pgcat_pub SET v = v + 1;
SELECT v FROM public.pgcat_pub;
-- a temp relation is reflected into pg_class
CREATE TEMP TABLE pgcat_tmp (z int);
SELECT relname, relnatts FROM pg_class WHERE relname = 'pgcat_tmp';
-- a qualified miss reports the schema in the 42P01 message, as PG does
SELECT * FROM pg_catalog.no_such_catalog;
-- writing a system catalog is refused
INSERT INTO pg_catalog.pg_type VALUES (1);
