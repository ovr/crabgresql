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
-- A view's columns carry the type modifier the query's output does: a plain
-- reference and an explicit cast keep theirs, anything computed loses it.
CREATE TABLE tm(a varchar(20), b numeric(8,2), c timestamp(3), d char(5),
                e interval day to second(2));
CREATE VIEW v_tm AS
  SELECT a, b, c, d, e,
         a || 'x' AS cat,
         (SELECT a) AS sq,
         a::varchar(9) AS cst,
         upper(a) AS up,
         'abc'::varchar(7) AS lit,
         CASE WHEN true THEN a ELSE a END AS arms
    FROM tm;
SELECT attname, format_type(atttypid, atttypmod) AS spelling
  FROM pg_attribute WHERE attrelid = 'v_tm'::regclass AND attnum > 0 ORDER BY attnum;
-- A set operation keeps a modifier only when every arm declares the same one.
CREATE VIEW v_tm_same AS SELECT a FROM tm UNION ALL SELECT a FROM tm;
SELECT attname, format_type(atttypid, atttypmod) AS spelling
  FROM pg_attribute WHERE attrelid = 'v_tm_same'::regclass AND attnum > 0;
-- CREATE TABLE AS carries them across the same way.
CREATE TABLE tm_copy AS SELECT a, b, c, d, e FROM tm;
SELECT attname, format_type(atttypid, atttypmod) AS spelling
  FROM pg_attribute WHERE attrelid = 'tm_copy'::regclass AND attnum > 0 ORDER BY attnum;
DROP VIEW v_tm_same;
DROP VIEW v_tm;
DROP TABLE tm_copy;
DROP TABLE tm;

-- A view names an unaliased target the way PostgreSQL's FigureColname does: the
-- operand names a cast over it, so a::text is "a" and only an expression that
-- names nothing takes the target type. The operand of a cast node is
-- parenthesised, and a subscript's index is deparsed rather than echoed.
CREATE TABLE tn (a int, arr int[], i int);
CREATE VIEW v_names AS
  SELECT a::text, upper(a::text), (a + 1)::text, arr[i + 1], ARRAY[arr[1], 2] FROM tn;
SELECT pg_get_viewdef('v_names'::regclass);
SELECT pg_get_viewdef('v_names'::regclass, true);
DROP VIEW v_names;
DROP TABLE tn;

-- A quantified comparison is never a "simple node" to PostgreSQL's pretty
-- printer -- `isSimpleNode` has no arm for a ScalarArrayOpExpr -- so it takes
-- parentheses wherever something encloses it, and none at a clause boundary.
-- Non-pretty wraps every operator node either way.
CREATE TABLE tq (a int, b text);
CREATE VIEW v_any AS SELECT a = ANY (ARRAY[1, 2]) AS c FROM tq
 WHERE a = ANY (ARRAY[1, 2]) AND NOT (b <> ALL (ARRAY['x']));
SELECT pg_get_viewdef('v_any'::regclass);
SELECT pg_get_viewdef('v_any'::regclass, true);
CREATE VIEW v_any_top AS SELECT * FROM tq WHERE a = ANY (ARRAY[1, 2]);
SELECT pg_get_viewdef('v_any_top'::regclass, true);
DROP VIEW v_any_top;
DROP VIEW v_any;
DROP TABLE tq;

-- A function FROM item deparses as the call it is, with the alias and column
-- list PostgreSQL synthesises for a function range table entry: a bare call is
-- named after the function, a bare alias renames the scalar column, an alias
-- list wins over it, and WITH ORDINALITY sits between the call and the alias.
CREATE VIEW v_fn AS SELECT * FROM abs(1);
CREATE VIEW v_fn_alias AS SELECT * FROM abs(1) t;
CREATE VIEW v_fn_cols AS SELECT * FROM abs(1) t(v);
CREATE VIEW v_fn_ord AS SELECT * FROM abs(1) WITH ORDINALITY;
CREATE VIEW v_fn_srf AS SELECT * FROM generate_series(1, 3);
CREATE VIEW v_fn_text AS SELECT * FROM upper('cd');
SELECT pg_get_viewdef('v_fn'::regclass);
SELECT pg_get_viewdef('v_fn_alias'::regclass);
SELECT pg_get_viewdef('v_fn_cols'::regclass);
SELECT pg_get_viewdef('v_fn_ord'::regclass);
SELECT pg_get_viewdef('v_fn_srf'::regclass);
SELECT pg_get_viewdef('v_fn_text'::regclass);
-- and the rendered definition reads back as the same view.
SELECT * FROM v_fn;
SELECT * FROM v_fn_ord;
DROP VIEW v_fn_text;
DROP VIEW v_fn_srf;
DROP VIEW v_fn_ord;
DROP VIEW v_fn_cols;
DROP VIEW v_fn_alias;
DROP VIEW v_fn;

-- Clean up (this suite shares one database across tests).
DROP VIEW v_over;
DROP VIEW v_alias;
DROP VIEW v;
DROP TABLE t;
