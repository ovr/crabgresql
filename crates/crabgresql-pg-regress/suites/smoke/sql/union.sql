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
-- error: each arm must have the same number of columns
SELECT id FROM un1 UNION SELECT id, big FROM un1;
-- error: the arm types must be unifiable
SELECT id FROM un1 UNION SELECT name FROM un1;
-- INTERSECT / EXCEPT are not supported yet
SELECT id FROM un1 INTERSECT SELECT id FROM un1;
SELECT id FROM un1 EXCEPT SELECT id FROM un1;
-- the session stays usable after the errors above
SELECT 'ok' AS status;
