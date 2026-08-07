--
-- CREATE TABLE AS
-- Smoke test for CREATE TABLE ... AS <query>: the new table's columns derive
-- from the query's target list, its rows are populated, the completion tag is
-- SELECT n, and the relation reflects as an ordinary table. (Table names are
-- prefixed `ctas` because the smoke suite shares one database across files.)
--
CREATE TABLE ctas_src (id integer, name text);
INSERT INTO ctas_src VALUES (1, 'one'), (2, 'two'), (3, 'three');
-- Basic CTAS: column names and types come from the SELECT target list.
CREATE TABLE ctas1 AS SELECT id, name FROM ctas_src WHERE id <= 2;
SELECT * FROM ctas1 ORDER BY id;
-- Expressions and aliases in the target list name and type the columns; the
-- source ORDER BY runs, but the new table has no inherent order.
CREATE TABLE ctas2 AS SELECT id * 10 AS big, name AS label FROM ctas_src ORDER BY id;
SELECT * FROM ctas2 ORDER BY big;
-- CTAS over a VALUES list names the columns column1, column2, ...
CREATE TABLE ctas3 AS VALUES (1, 'a'), (2, 'b');
SELECT * FROM ctas3 ORDER BY column1;
-- An empty source creates the table but inserts no rows (SELECT 0).
CREATE TABLE ctas_empty AS SELECT id FROM ctas_src WHERE false;
SELECT * FROM ctas_empty;
-- Re-creating an existing relation fails; IF NOT EXISTS downgrades to a notice
-- and runs nothing, so the original rows are untouched.
CREATE TABLE ctas1 AS SELECT 9 AS id, 'nine' AS name;
CREATE TABLE IF NOT EXISTS ctas1 AS SELECT 9 AS id, 'nine' AS name;
SELECT * FROM ctas1 ORDER BY id;
-- The new relations reflect as ordinary tables (relkind 'r').
SELECT relname, relkind FROM pg_class WHERE relname LIKE 'ctas%' ORDER BY relname;
