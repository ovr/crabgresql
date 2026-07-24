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
-- pg_am lists PostgreSQL's built-in access methods
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
