--
-- CREATE VIEW
-- A view is a stored query: SELECT expands it, an explicit column list renames
-- its outputs, a view may read another view, OR REPLACE swaps the definition
-- (and may only add trailing columns), and it reflects into the catalogs as
-- relkind 'v'. Output hand-checked against PostgreSQL (psql -a -q).
--
CREATE TABLE t (id integer, name text);
INSERT INTO t VALUES (1, 'one'), (2, 'two'), (3, 'three');
CREATE VIEW v AS SELECT id, name FROM t WHERE id < 3;
SELECT id, name FROM v ORDER BY id;
-- An explicit column list renames the view's output columns.
CREATE VIEW v_alias (num, label) AS SELECT id, name FROM t;
SELECT num, label FROM v_alias ORDER BY num;
-- A view can select from another view.
CREATE VIEW v_over AS SELECT label FROM v_alias WHERE num = 2;
SELECT label FROM v_over;
-- OR REPLACE swaps the body; adding a trailing column is allowed.
CREATE OR REPLACE VIEW v AS SELECT id, name, id * 10 AS scaled FROM t WHERE id < 3;
SELECT id, name, scaled FROM v ORDER BY id;
-- A plain re-create of an existing name is an error.
CREATE VIEW v AS SELECT 1;
-- IF NOT EXISTS downgrades that to a skip NOTICE.
CREATE VIEW IF NOT EXISTS v AS SELECT 1;
-- A table cannot take a view's name either.
CREATE TABLE v (x integer);
-- Views reflect into pg_class as relkind 'v'.
SELECT relname, relkind FROM pg_class WHERE relname IN ('t', 'v') ORDER BY relname;
-- ... and into information_schema.tables as VIEW.
SELECT table_name, table_type FROM information_schema.tables
  WHERE table_name IN ('v', 'v_alias') ORDER BY table_name;
-- Clean up (this suite shares one database across tests).
DROP VIEW v_over;
DROP VIEW v_alias;
DROP VIEW v;
DROP TABLE t;
