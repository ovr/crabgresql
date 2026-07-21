--
-- DROP SCHEMA
-- DROP SCHEMA defaults to RESTRICT: a non-empty schema blocks the drop (2BP01);
-- CASCADE drops the schema and its contents. A missing schema errors (3F000);
-- IF EXISTS downgrades that to a skip NOTICE. Output hand-checked against
-- PostgreSQL (psql -a -q).
--
CREATE SCHEMA app;
CREATE TABLE app.t (id integer);
-- RESTRICT (the default): the contained table blocks the drop.
DROP SCHEMA app;
-- CASCADE drops the schema and everything in it.
DROP SCHEMA app CASCADE;
-- Afterwards the schema is gone.
SELECT nspname FROM pg_namespace WHERE nspname = 'app';
-- A missing schema without IF EXISTS errors; with IF EXISTS it is a skip NOTICE.
DROP SCHEMA app;
DROP SCHEMA IF EXISTS app;
-- An empty schema drops cleanly under the default RESTRICT.
CREATE SCHEMA empty_one;
DROP SCHEMA empty_one;
