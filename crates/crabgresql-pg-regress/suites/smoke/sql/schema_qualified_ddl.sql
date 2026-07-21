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
-- A serial column and a standalone sequence both live in the schema; their
-- defaults and explicit sequence functions resolve within it.
CREATE TABLE app.counter (id serial, note text);
INSERT INTO app.counter (note) VALUES ('a'), ('b');
SELECT id, note FROM app.counter ORDER BY id;
CREATE SEQUENCE app.seq;
SELECT nextval('app.seq');
SELECT nextval('app.seq');
SELECT currval('app.seq');
SELECT setval('app.seq', 10);
SELECT nextval('app.seq');
-- A qualified PRIMARY KEY builds its index in the schema.
CREATE TABLE app.keyed (id integer PRIMARY KEY);
INSERT INTO app.keyed VALUES (1);
DROP SCHEMA app CASCADE;
DROP TABLE item;
