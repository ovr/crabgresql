--
-- SCHEMA-QUALIFIED TYPE NAMES
-- Built-in types live in pg_catalog, so `pg_catalog.int4` and a bare `int4`
-- name the same type. psql's \d leans on the qualified spelling throughout
-- (`::pg_catalog.text`, `::pg_catalog.int2[]`), including inside array types.
--
-- the qualified and bare spellings are the same type, and name the column alike
SELECT 1::pg_catalog.int4 AS qualified, 1::int4 AS bare;
-- every builtin answers to its catalog spelling under the qualifier
SELECT 'a'::pg_catalog.text AS t, 1::pg_catalog.int8 AS i8, true::pg_catalog.bool AS b;
-- the SQL spelling of a type resolves the same way when written bare
SELECT 1::integer AS spelled_out, 1.5::decimal AS dec;
-- a qualified element type inside an array type
SELECT ARRAY[1,2]::pg_catalog.int2[] AS arr;
-- casting through a qualified name in a WHERE clause
CREATE TABLE qt (id integer, label text);
INSERT INTO qt VALUES (1, 'one'), (2, 'two');
SELECT label FROM qt WHERE id = '2'::pg_catalog.int4;
-- and in a column definition
CREATE TABLE qt2 (a pg_catalog.int4, b pg_catalog.text);
INSERT INTO qt2 VALUES (7, 'seven');
SELECT a, b FROM qt2;
-- A name that is not a builtin still falls through to the user-type catalog,
-- and a builtin stays reachable alongside it.
CREATE TYPE qmood AS ENUM ('ok', 'bad');
SELECT 'ok'::qmood AS user_type;
SELECT 1::pg_catalog.int4 AS builtin_still;
-- A qualifier other than pg_catalog does not reach a builtin: it names a type
-- in that schema. Two cases are left out of this suite as known divergences —
-- `SELECT 1::public.int4` errors in both, but PG reports the quirky
-- `type "public.int4" is only a shell`; and a qualified *user* type name
-- (`CREATE TYPE qs.int4` / `'x'::qs.int4`) is not supported yet at all.
DROP TABLE qt;
DROP TABLE qt2;
DROP TYPE qmood;
