--
-- Subqueries in expressions
-- Non-correlated scalar subqueries, EXISTS, and IN (SELECT ...): each subplan
-- runs once and folds into the surrounding expression. Correlated subqueries
-- (referencing the outer row) are not covered here.
--
CREATE TABLE sq (id integer, val integer);
INSERT INTO sq VALUES (1, 10), (2, 20), (3, 30);
-- scalar subquery in the target list
SELECT (SELECT max(val) FROM sq) AS max_val;
-- a scalar subquery returning no rows is NULL
SELECT (SELECT val FROM sq WHERE id = 99) AS no_rows;
-- scalar subquery in a WHERE comparison, evaluated once
SELECT id FROM sq WHERE val > (SELECT min(val) FROM sq) ORDER BY id;
-- EXISTS is true when the subquery yields any row
SELECT EXISTS (SELECT 1 FROM sq WHERE val = 20) AS has_20,
       EXISTS (SELECT 1 FROM sq WHERE val = 99) AS has_99;
-- NOT EXISTS negates it
SELECT id FROM sq WHERE NOT EXISTS (SELECT 1 FROM sq WHERE val > 100) ORDER BY id;
-- IN (SELECT ...): membership against the subquery's single column
SELECT id FROM sq WHERE val IN (SELECT val FROM sq WHERE val <> 20) ORDER BY id;
-- NOT IN (SELECT ...)
SELECT id FROM sq WHERE val NOT IN (SELECT val FROM sq WHERE val = 20) ORDER BY id;
-- IN with a NULL in the candidate set: three-valued logic (no match -> NULL)
SELECT 5 IN (SELECT val FROM (VALUES (10), (NULL)) AS v(val)) AS in_null_nomatch;
-- a real match short-circuits the NULL and returns true
SELECT 10 IN (SELECT val FROM (VALUES (10), (NULL)) AS v(val)) AS in_null_match;
-- a scalar subquery in the SELECT list, combined with a column
SELECT id, id + (SELECT count(*) FROM sq) AS plus_count FROM sq ORDER BY id;
-- error: a scalar subquery that returns more than one row
SELECT (SELECT val FROM sq);
