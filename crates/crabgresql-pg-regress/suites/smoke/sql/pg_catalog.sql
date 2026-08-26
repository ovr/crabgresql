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
 WHERE nspname IN ('pg_catalog', 'pg_toast', 'public', 'information_schema')
 ORDER BY oid;
SELECT 'information_schema'::regnamespace AS by_name,
       13699::regnamespace AS by_oid,
       has_schema_privilege('information_schema', 'USAGE') AS usage;
SELECT 'information_schema.tables'::regclass AS by_name,
       13916::regclass AS by_oid,
       pg_table_is_visible('information_schema.tables'::regclass) AS visible,
       has_table_privilege('information_schema.tables', 'SELECT') AS may_read;
-- a served view is a relation for every by-OID lookup too: its columns exist,
-- it owns no sequence, and it measures zero rather than nothing at all
SELECT has_column_privilege('information_schema.tables', 'table_name', 'SELECT') AS may_read,
       pg_get_serial_sequence('information_schema.tables', 'table_name') AS owns_seq,
       pg_relation_size('information_schema.tables'::regclass) AS size;
SELECT pg_get_serial_sequence('information_schema.tables', 'no_such_column');
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
-- size bookkeeping is only written by ANALYZE, so a fresh relation reports
-- PG's never-analyzed sentinel: no pages, reltuples = -1 (unknown, not zero)
SELECT relname, relpages, reltuples, relallvisible
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
-- the ACL columns are present and NULL everywhere: nothing here GRANTs, and
-- NULL is what PostgreSQL reports for an object still on its owner's defaults
SELECT count(*) AS with_acl FROM pg_class WHERE relacl IS NOT NULL;
SELECT count(*) AS with_acl FROM pg_type WHERE typacl IS NOT NULL;
SELECT count(*) AS with_acl FROM pg_proc WHERE proacl IS NOT NULL;
-- the storage columns answer rather than erroring: no storage parameter is
-- kept, and no visibility map is built
SELECT relname, relallfrozen, relisshared, relispopulated, relrewrite,
       relfrozenxid, relminmxid, reloptions
  FROM pg_class
 WHERE relname = 'pgcat_reflect';
-- only a relation holding unfrozen tuples carries a freeze horizon, so the
-- partitioned parent reports 0 and so does the sequence, whose one tuple
-- PostgreSQL writes frozen
CREATE TABLE pgcat_parent (k int) PARTITION BY RANGE (k);
CREATE TABLE pgcat_leaf PARTITION OF pgcat_parent FOR VALUES FROM (0) TO (10);
CREATE SEQUENCE pgcat_seq;
SELECT relname, relkind, relhassubclass, relispartition, relfrozenxid
  FROM pg_class
 WHERE relname IN ('pgcat_parent', 'pgcat_leaf', 'pgcat_seq')
 ORDER BY relname;
-- dropped again: the whole suite shares one database, and a live partition
-- here would show up in every later test that lists pg_inherits
DROP TABLE pgcat_parent;
DROP SEQUENCE pgcat_seq;
-- no routine declares an argument default, which is what pronargdefaults = 0
-- says too
SELECT count(*) AS with_defaults FROM pg_proc WHERE proargdefaults IS NOT NULL;
SELECT count(*) AS with_config FROM pg_proc WHERE proconfig IS NOT NULL;
SELECT proname, pronargdefaults, proargdefaults, protrftypes, prosqlbody
  FROM pg_proc
 WHERE proname = 'int4pl';
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
