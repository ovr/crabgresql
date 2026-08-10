--
-- COALESCE
-- The first non-NULL argument: laziness, result-type resolution over any
-- arity, and the error cases, checked against psql's aligned format.
--
-- the default column name is "coalesce"
SELECT COALESCE(NULL, 'fallback');
-- the first non-NULL wins, whatever follows it
SELECT COALESCE(NULL, 1, 2) AS first_non_null;
-- one argument is legal, and is just that argument
SELECT COALESCE(7) AS single;
-- every argument NULL yields NULL (renders empty); an all-untyped list resolves
-- to text, as it does for CASE/UNION/VALUES
SELECT COALESCE(NULL, NULL) AS none, pg_typeof(COALESCE(NULL, NULL)) AS ty;
-- arguments of differing numeric types resolve to the common type
SELECT COALESCE(1, 2.5) AS promoted, pg_typeof(COALESCE(1, 2.5)) AS ty;
SELECT pg_typeof(COALESCE(1::int, 2::bigint)) AS ty;
-- an untyped literal adapts to the resolved type per argument
SELECT COALESCE(NULL::int, '42') + 1 AS added;
-- arguments after the first non-NULL one are never evaluated, so a division by
-- zero that can never be reached is not an error
SELECT COALESCE(1, 1/0) AS lazy;
-- ... but it is reached when the arguments before it are all NULL
SELECT COALESCE(NULL::int, 1/0);
-- COALESCE over a table column, once per row
CREATE TABLE readings (station text, temp integer);
INSERT INTO readings VALUES ('a', 12), ('b', NULL), ('c', -3);
SELECT station, COALESCE(temp, 0) AS temp FROM readings ORDER BY station;
-- in a WHERE predicate, and under an aggregate
SELECT station FROM readings WHERE COALESCE(temp, 0) >= 0 ORDER BY station;
SELECT sum(COALESCE(temp, 0)) AS total FROM readings;
-- as a GROUP BY key
SELECT COALESCE(temp, 0) AS temp, count(*) AS n
  FROM readings GROUP BY COALESCE(temp, 0) ORDER BY 1;
-- error: incompatible concrete argument types cannot be matched. Known gap: PG
-- also prints a `LINE 1: ... ^` cursor under the offending argument, which the
-- result-type unifier does not carry a span for yet.
SELECT COALESCE(1, 'abc'::text);
-- error: an untyped argument that does not fit the resolved type
SELECT COALESCE(1, 'x');
-- error: at least one argument is required. PG rejects this in the grammar and
-- prints the same message, with a cursor under the paren
SELECT COALESCE();
-- error: two explicit collations that disagree (PG prints a cursor here too)
SELECT COALESCE('a' COLLATE "C", 'b' COLLATE "POSIX");
-- error: COALESCE is a grammar construct, not a function in a schema, so a
-- schema-qualified call finds nothing. Known gap: PG names the missing function
-- `pg_catalog.coalesce(integer, integer)`, and adds a cursor and a HINT.
SELECT pg_catalog.coalesce(1, 2);
DROP TABLE readings;
