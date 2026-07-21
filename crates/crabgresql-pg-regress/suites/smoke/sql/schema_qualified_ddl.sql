--
-- SCHEMA-QUALIFIED DDL
-- A schema-qualified relation lives in its own namespace, coexisting with a
-- same-named relation in public; its pg_class.relnamespace resolves to the
-- schema's pg_namespace.oid. Creating in a missing schema errors (3F000).
-- Output hand-checked against PostgreSQL (psql -a -q).
--
CREATE SCHEMA app;
CREATE TABLE app.item (id integer, label text);
CREATE TABLE item (id integer);
INSERT INTO app.item VALUES (1, 'a'), (2, 'b');
INSERT INTO item VALUES (99);
-- The qualified name reads its own rows, distinct from public.item.
SELECT id, label FROM app.item ORDER BY id;
SELECT id FROM item ORDER BY id;
-- Each item's relnamespace points at the schema it lives in.
SELECT n.nspname
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relname = 'item'
ORDER BY n.nspname;
-- Creating in a schema that does not exist errors.
CREATE TABLE nope.t (id integer);
DROP SCHEMA app CASCADE;
DROP TABLE item;
