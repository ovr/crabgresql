--
-- SELECT DISTINCT
-- Row deduplication over the visible output columns, keeping one row per
-- distinct combination. ORDER BY (whose keys must all be select-list columns)
-- sorts the deduplicated result; two NULLs collapse into one row.
--
CREATE TABLE dist (id integer, a integer, b integer, name text);
INSERT INTO dist VALUES
  (1, 1, 10, 'ferris'),
  (2, 1, 10, 'ferris'),
  (3, 2, 20, 'hermit'),
  (4, 2, 30, 'hermit'),
  (5, 3, 30, NULL),
  (6, 3, 30, NULL);
-- distinct on a single column
SELECT DISTINCT a FROM dist ORDER BY a;
-- distinct over multiple columns
SELECT DISTINCT a, b FROM dist ORDER BY a, b;
-- NULLs collapse to a single distinct row
SELECT DISTINCT name FROM dist ORDER BY name;
-- distinct over an expression in the select list
SELECT DISTINCT a + b AS total FROM dist ORDER BY total;
-- SELECT ALL keeps duplicates (the default)
SELECT ALL a FROM dist ORDER BY a;
-- distinct with no ORDER BY still deduplicates
SELECT DISTINCT a, name FROM dist ORDER BY a, name;
-- error: an ORDER BY key that is not in the select list
SELECT DISTINCT a FROM dist ORDER BY b;
DROP TABLE dist;
