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
-- A serial column's OWNED BY sequence is an internal dependency: CASCADE lists
-- only the table, not the sequence (which is dropped with it).
CREATE SCHEMA app;
CREATE TABLE app.serial_tab (id serial, note text);
DROP SCHEMA app CASCADE;
-- Qualified DROP TABLE honors dependent views across schemas: a public view on a
-- schema-qualified table blocks RESTRICT and is cascaded, and a repeated target
-- is rejected before anything is dropped.
CREATE SCHEMA app;
CREATE TABLE app.base (id integer);
CREATE VIEW pubv AS SELECT id FROM app.base;
DROP TABLE app.base;
DROP TABLE app.base, app.base;
DROP TABLE app.base CASCADE;
DROP SCHEMA app;
-- a schema the database system requires cannot be dropped, and IF EXISTS does
-- not forgive it: that clause covers a schema that is absent
DROP SCHEMA pg_catalog;
DROP SCHEMA IF EXISTS pg_toast;
