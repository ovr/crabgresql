--
-- GENERATE_SUBSCRIPTS
-- The set-returning function generate_subscripts over an array's dimension, in
-- both the target list and FROM position. Output hand-checked against
-- PostgreSQL's aligned format.
--
-- target-list form: one row per valid subscript, the column named after the
-- function
SELECT generate_subscripts(ARRAY['a', 'b', 'c'], 1);
SELECT pg_typeof(s) FROM generate_subscripts(ARRAY['a'], 1) s;
-- FROM position
SELECT * FROM generate_subscripts(ARRAY['a', 'b', 'c'], 1);
-- reverse yields them descending; an explicit false does not
SELECT generate_subscripts(ARRAY['a', 'b', 'c'], 1, true);
SELECT generate_subscripts(ARRAY['a', 'b', 'c'], 1, false);
-- a bare alias renames the single column, and a column list wins over it
SELECT g FROM generate_subscripts(ARRAY[10, 20], 1) g;
SELECT s FROM generate_subscripts(ARRAY[10, 20], 1) g(s);
-- WITH ORDINALITY appends the ordinal, as it does for any set-returning
-- function
SELECT * FROM generate_subscripts(ARRAY['x', 'y'], 1) WITH ORDINALITY;
-- the idiom the function exists for: subscript alongside element
CREATE TABLE arr (id int, a text[]);
INSERT INTO arr VALUES (1, ARRAY['x', 'y', 'z']), (2, ARRAY['p']);
SELECT id, i, a[i] AS v FROM arr, generate_subscripts(ARRAY['x', 'y', 'z'], 1) i
  WHERE id = 1 ORDER BY i;
-- over a stored array in the target list, both directions
SELECT id, generate_subscripts(a, 1) AS s FROM arr ORDER BY id, s;
SELECT id, generate_subscripts(a, 1, true) AS s FROM arr ORDER BY id, s DESC;
-- a dimension the array does not have yields no rows rather than an error;
-- the engine's arrays are one-dimensional, so that is every dimension but 1
SELECT generate_subscripts(ARRAY['a', 'b'], 0);
SELECT generate_subscripts(ARRAY['a', 'b'], 2);
SELECT generate_subscripts(ARRAY['a', 'b'], -1);
-- an empty array has no subscripts at all
SELECT generate_subscripts('{}'::text[], 1);
-- the function is strict: a NULL in any argument yields no rows
SELECT generate_subscripts(NULL::text[], 1);
SELECT generate_subscripts(ARRAY['a', 'b'], NULL);
SELECT generate_subscripts(ARRAY['a', 'b'], 1, NULL);
-- oidvector/int2vector are subscripted from 0, so their subscripts start there
SELECT generate_subscripts('11 22 33'::oidvector, 1);
SELECT generate_subscripts('11 22'::int2vector, 1);
-- a smallint dimension widens to int4 and binds
SELECT generate_subscripts(ARRAY['a', 'b'], 1::smallint);
-- only (anyarray, int) and (anyarray, int, boolean) resolve
SELECT generate_subscripts(ARRAY['a', 'b']);
SELECT generate_subscripts(ARRAY['a', 'b'], 1::bigint);
SELECT generate_subscripts(ARRAY['a', 'b'], 1.5);
SELECT generate_subscripts(1, 1);
SELECT generate_subscripts(ARRAY['a', 'b'], 1, true, 4);
-- the dimension is an int4, so a literal its input function rejects is a
-- value error and not an overload one
SELECT generate_subscripts(ARRAY['a', 'b'], 'x');
DROP TABLE arr;
