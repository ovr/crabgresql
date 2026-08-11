--
-- UNION / UNION ALL
-- Concatenation (UNION ALL) and set-union deduplication (UNION), with per
-- position type unification, output column names taken from the first arm, and
-- a query-level ORDER BY over the combined result. INTERSECT / EXCEPT are not
-- supported yet.
--
CREATE TABLE un1 (id integer, big bigint, name text);
INSERT INTO un1 VALUES (1, 100, 'a'), (2, 200, 'b'), (2, 200, 'b');
-- UNION ALL keeps duplicates from both arms
SELECT id FROM un1 UNION ALL SELECT id FROM un1 ORDER BY 1;
-- UNION removes duplicates across both arms
SELECT id FROM un1 UNION SELECT id FROM un1 ORDER BY 1;
-- explicit DISTINCT is the default quantifier
SELECT id FROM un1 UNION DISTINCT SELECT id FROM un1 ORDER BY 1;
-- integer and bigint arms unify to bigint
SELECT id FROM un1 UNION ALL SELECT big FROM un1 ORDER BY 1;
-- three-way UNION deduplicates across all three arms
SELECT 1 UNION SELECT 2 UNION SELECT 1 ORDER BY 1;
-- the result column name comes from the first (left) arm
SELECT id AS ident FROM un1 UNION SELECT big FROM un1 ORDER BY 1;
-- a UNION ALL arm may be a VALUES list
SELECT name FROM un1 UNION ALL VALUES ('y'), ('z') ORDER BY 1;
-- an untyped NULL arm takes its type from the other arm (the padding idiom)
SELECT id FROM un1 UNION ALL SELECT NULL ORDER BY 1;
SELECT id, name FROM un1 UNION ALL SELECT NULL, NULL ORDER BY 1;
-- a column that is NULL in every arm falls back to text
SELECT NULL UNION ALL SELECT NULL;
-- the string types are mutually convertible, so the column takes the *first* arm's
-- type rather than the category's preferred `text`: char and varchar arms meet at
-- char, and the padding of the char arm survives
CREATE TABLE un2 (v varchar(5), c char(5));
INSERT INTO un2 VALUES ('ab', 'cd');
SELECT c FROM un2 UNION ALL SELECT v FROM un2 ORDER BY 1;
SELECT v FROM un2 UNION ALL SELECT c FROM un2 ORDER BY 1;
-- a correlated reference inside a UNION arm resolves per outer row
SELECT id, (SELECT o.id UNION SELECT o.id) AS same FROM un1 o ORDER BY 1;
SELECT id FROM un1 o WHERE EXISTS (SELECT o.id UNION ALL SELECT o.big) ORDER BY 1;
-- a parenthesized arm carries its own WITH
(WITH w AS (SELECT 1 AS n) SELECT n FROM w) UNION ALL SELECT 2 ORDER BY 1;
-- a parenthesized set operation takes a query-level ORDER BY / LIMIT
(SELECT id FROM un1 UNION SELECT id FROM un1) ORDER BY 1;
(SELECT id FROM un1 UNION SELECT id FROM un1 ORDER BY 1) LIMIT 1;
-- UNION ALL over a deduplicating arm keeps the inner deduplication
SELECT count(*) FROM ((SELECT id FROM un1 UNION SELECT id FROM un1)
  UNION ALL SELECT id FROM un1) x;
-- error: each arm must have the same number of columns
SELECT id FROM un1 UNION SELECT id, big FROM un1;
-- error: the arm types must be unifiable
SELECT id FROM un1 UNION SELECT name FROM un1;
-- error: ORDER BY on a set operation names a result column
SELECT id FROM un1 UNION SELECT id FROM un1 ORDER BY nosuch;
SELECT id FROM un1 UNION SELECT id FROM un1 ORDER BY id + 1;
-- error: UNION needs an equality operator for every column
SELECT '{"a":1}'::json UNION SELECT '{"b":2}'::json;
-- INTERSECT / EXCEPT are not supported yet
SELECT id FROM un1 INTERSECT SELECT id FROM un1;
SELECT id FROM un1 EXCEPT SELECT id FROM un1;
-- the session stays usable after the errors above
SELECT 'ok' AS status;
