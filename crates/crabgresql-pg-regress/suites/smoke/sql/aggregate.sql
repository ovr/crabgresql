--
-- AGGREGATE
-- The five standard aggregates (count, min, max, sum, avg) over a whole table
-- and with GROUP BY / HAVING. Covers NULL handling (ignored, except count(*)),
-- empty-group semantics (count 0, others NULL), sum/avg output types, and the
-- grouping errors PG reports for ungrouped columns and misplaced aggregates.
--
CREATE TABLE agg (id integer, grp integer, val integer, big bigint, txt text);
INSERT INTO agg VALUES
  (1, 1, 10, 1000000000000, 'apple'),
  (2, 1, 20, 2000000000000, 'banana'),
  (3, 2, 5, 3000000000000, NULL),
  (4, 2, NULL, NULL, 'cherry'),
  (5, 3, 7, 4000000000000, 'date');
-- whole-table aggregates: count(*) counts rows, count(val) skips the NULL
SELECT count(*), count(val), count(txt) FROM agg;
-- min/max/sum ignore NULL; sum(int) widens to bigint; avg(int) is numeric
SELECT min(val), max(val), sum(val), avg(val) FROM agg;
-- min/max over text
SELECT min(txt), max(txt) FROM agg;
-- sum(bigint) is numeric, avg(bigint) is numeric
SELECT sum(big), avg(big) FROM agg;
-- an aggregate expression and an alias
SELECT max(val) - min(val) AS span FROM agg;
-- a bare constant alongside an aggregate
SELECT 'total' AS label, count(*) FROM agg;
-- GROUP BY with ORDER BY on the grouping column
SELECT grp, count(*), sum(val), min(val), max(val) FROM agg GROUP BY grp ORDER BY grp;
-- a compound expression over the grouping column
SELECT grp, grp * 10 AS scaled, count(*) FROM agg GROUP BY grp ORDER BY 1;
-- GROUP BY ordinal
SELECT grp, count(*) FROM agg GROUP BY 1 ORDER BY 1;
-- HAVING filters groups after aggregation
SELECT grp, count(*) FROM agg GROUP BY grp HAVING count(*) > 1 ORDER BY grp;
-- GROUP BY with no aggregate is a distinct
SELECT grp FROM agg GROUP BY grp ORDER BY grp;
-- WHERE filters before aggregation; an empty group still yields one row
SELECT count(*), sum(val) FROM agg WHERE id > 100;
-- a FROM-less aggregate runs over the single virtual row
SELECT count(*);
-- errors: an ungrouped column may not appear outside an aggregate
SELECT grp, count(*) FROM agg;
-- errors: aggregates are not allowed in WHERE
SELECT count(*) FROM agg WHERE count(*) > 1;
-- errors: aggregate calls cannot be nested
SELECT max(min(val)) FROM agg;
-- errors: no sum over text
SELECT sum(txt) FROM agg;
-- DISTINCT eliminates duplicate non-NULL aggregate inputs
SELECT count(DISTINCT grp), sum(DISTINCT grp), avg(DISTINCT grp), min(DISTINCT grp), max(DISTINCT grp) FROM agg;
-- sum/avg over bigint accumulate past what a bigint can hold, and the quotient's
-- display scale falls to zero at that magnitude
CREATE TABLE wide (v bigint);
INSERT INTO wide VALUES (9223372036854775807), (9223372036854775807), (9223372036854775807);
SELECT sum(v), avg(v) FROM wide;
-- End aggregate tests.
