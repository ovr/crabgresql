--
-- ARRAY_AGG
-- The one aggregate that does not skip a NULL input: a NULL row becomes a NULL
-- element, while a group with no rows at all is still NULL rather than the empty
-- array. Covers the element types, GROUP BY / HAVING / ORDER BY, the DISTINCT
-- form (which PostgreSQL sorts, NULLs last), the window form, and the two shapes
-- this build refuses. Generated with psql -q -a against PostgreSQL 18.4.
--
CREATE TABLE aa (id integer, grp integer, val integer, txt text);
INSERT INTO aa VALUES
  (1, 1, 10, 'apple'),
  (2, 1, NULL, 'banana'),
  (3, 2, 5, NULL),
  (4, 2, 5, 'cherry'),
  (5, 3, 7, 'date');
-- NULL inputs are kept, and the elements follow the scan order
SELECT array_agg(val) FROM aa;
SELECT array_agg(txt) FROM aa;
-- an empty group is NULL, not {}
SELECT array_agg(val) FROM aa WHERE false;
-- a group of one NULL row is {NULL}, which is what the empty group differs from
SELECT array_agg(val) FROM aa WHERE id = 2;
-- the result type is the array over the argument type
SELECT pg_typeof(array_agg(val)), pg_typeof(array_agg(txt)) FROM aa;
SELECT pg_typeof(array_agg(val::smallint)), pg_typeof(array_agg(val::bigint)) FROM aa;
-- element types across the board; numeric keeps its display scale
SELECT array_agg(v) FROM (VALUES (1::numeric), (2.50), (NULL)) t(v);
SELECT array_agg(v) FROM (VALUES (1.5::float8), ('NaN'::float8)) t(v);
SELECT array_agg(v) FROM (VALUES (true), (false), (NULL)) t(v);
SELECT array_agg(v) FROM (VALUES ('2001-02-03'::date), (NULL)) t(v);
SELECT array_agg(v) FROM (VALUES ('2001-02-03 04:05:06'::timestamp)) t(v);
-- array_out quoting: a comma, an empty string and the literal word NULL are quoted
SELECT array_agg(v) FROM (VALUES ('a'), ('b, c'), (''), ('NULL'), (NULL)) t(v);
-- an expression argument, not just a column
SELECT array_agg(val * 2) FROM aa;
-- GROUP BY, one array per group
SELECT grp, array_agg(val) FROM aa GROUP BY grp ORDER BY grp;
SELECT grp, array_agg(txt) FROM aa GROUP BY grp ORDER BY grp;
-- HAVING over the aggregate's own argument
SELECT grp, array_agg(val) FROM aa GROUP BY grp HAVING count(val) > 1 ORDER BY grp;
-- ORDER BY the aggregate: arrays compare element-wise, shorter is less on a tie
SELECT grp, array_agg(val) FROM aa GROUP BY grp ORDER BY 2;
-- DISTINCT: PostgreSQL sorts the result and puts NULLs last
SELECT array_agg(DISTINCT val) FROM aa;
SELECT array_agg(DISTINCT txt) FROM aa;
SELECT grp, array_agg(DISTINCT val) FROM aa GROUP BY grp ORDER BY grp;
-- alongside the other aggregates over the same group
SELECT grp, count(*), sum(val), array_agg(val) FROM aa GROUP BY grp ORDER BY grp;
-- the window form: the default frame makes it a running array
SELECT id, array_agg(val) OVER (ORDER BY id) FROM aa ORDER BY id;
SELECT id, array_agg(txt) OVER (PARTITION BY grp ORDER BY id) FROM aa ORDER BY id;
-- an array argument would need a two-dimensional result, which this build has no
-- representation for
-- (PostgreSQL answers {{10},{NULL},{5},{5},{7}} here.)
SELECT array_agg(ARRAY[val]) FROM aa;
-- per-aggregate ORDER BY is not implemented
-- (PostgreSQL answers {5,5,7,10,NULL} here.)
SELECT array_agg(val ORDER BY val) FROM aa;
-- End array_agg tests.
