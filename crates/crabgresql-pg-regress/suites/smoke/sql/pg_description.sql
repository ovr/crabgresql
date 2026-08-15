--
-- pg_description: the comments PostgreSQL ships on its own built-in objects,
-- and the two functions that read them.
--
-- Only objects this build genuinely has are described, so every row below is
-- one PostgreSQL prints identically: the descriptions come from the same
-- catalog .dat files, vendored. What is NOT compared is a count — PostgreSQL
-- carries some 5400 rows against this build's ~640, because most of the rest
-- describe pg_operator, pg_conversion and the collations initdb finds, none of
-- which are relations this build serves.
--
-- Generated with psql -q -a against PostgreSQL 18.4.
--
-- A type, a function, an access method, a language and a schema: one per
-- catalog that carries bootstrap descriptions.
SELECT obj_description(16, 'pg_type') AS bool_type;
SELECT obj_description('boolin'::regproc::oid, 'pg_proc') AS boolin;
SELECT obj_description(oid, 'pg_am') AS heap_am FROM pg_am WHERE amname = 'heap';
SELECT obj_description(oid, 'pg_language') AS sql_language FROM pg_language WHERE lanname = 'sql';
SELECT obj_description('pg_catalog'::regnamespace::oid, 'pg_namespace') AS pg_catalog_schema;
-- The rows themselves, read straight out of the relation: objsubid is 0
-- because bootstrap data describes whole objects, never columns.
SELECT objsubid, description FROM pg_description
 WHERE classoid = 'pg_type'::regclass AND objoid = 16;
-- The deprecated one-argument form searches every catalog at once.
SELECT obj_description(16) AS any_catalog;
-- A catalog name no pg_catalog relation answers to is NULL, not an error: the
-- classoid comes from a sub-select, and a sub-select matching nothing is NULL.
SELECT obj_description(16, 'pg_bogus') IS NULL AS unknown_catalog_is_null;
-- An OID nothing describes, likewise.
SELECT obj_description(999999, 'pg_type') IS NULL AS unknown_oid_is_null;
-- Strict in every argument, including the ones past the first.
SELECT obj_description(NULL, 'pg_type') IS NULL AS null_oid,
       obj_description(16, NULL) IS NULL AS null_catalog,
       col_description(16, NULL) IS NULL AS null_column;
-- col_description finds nothing anywhere: neither server has a column comment
-- until someone writes one with COMMENT ON, which is why \d+ shows an empty
-- Description column for a system catalog on both.
SELECT col_description('pg_type'::regclass, 1) IS NULL AS no_column_comment;
SELECT count(*) AS column_comments FROM pg_description WHERE objsubid <> 0;
