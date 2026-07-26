--
-- JOIN_PUSHDOWN
-- A join condition written in WHERE reaches the join itself, so a comma join
-- executes the same way the equivalent JOIN ... ON does, and single-relation
-- restrictions filter their own scan. None of that may change a result: this
-- file pins the rows, and the outer-join cases pin the boundaries where moving
-- a predicate would change the answer. (The plan shapes themselves are asserted
-- in the planner's unit tests — EXPLAIN here is deliberately reduced and would
-- not match PG's cost-annotated output.)
--
CREATE TABLE p_a (id integer, v integer);
INSERT INTO p_a VALUES (1, 10), (2, 20), (3, 30);
CREATE TABLE p_b (id integer, w integer);
INSERT INTO p_b VALUES (2, 200), (3, 300), (4, 400);
CREATE TABLE p_c (id integer, z integer);
INSERT INTO p_c VALUES (3, 3000), (4, 4000);
-- a comma join with the condition in WHERE
SELECT a.v, b.w FROM p_a a, p_b b WHERE a.id = b.id ORDER BY 1;
-- the explicit form must agree row for row
SELECT a.v, b.w FROM p_a a JOIN p_b b ON a.id = b.id ORDER BY 1;
-- three-way: every level gets a key, no reordering needed
SELECT a.v, b.w, c.z FROM p_a a, p_b b, p_c c WHERE a.id = b.id AND b.id = c.id;
-- single-relation restrictions sink to the scan they restrict
SELECT a.v, b.w FROM p_a a, p_b b WHERE a.id = b.id AND a.v > 10 AND b.w < 300;
-- a non-equi condition yields no key and rides the nested loop
SELECT a.v, b.w FROM p_a a, p_b b WHERE a.v * 10 < b.w ORDER BY 1, 2;
-- duplicate keys on both sides still produce the full set of pairings
SELECT a.v, b.w FROM p_a a, p_b b WHERE a.id > 1 AND b.id > 2 ORDER BY 1, 2;
-- a bushy FROM: the right group is its own join tree with its own index space
SELECT a.v, b.w, c.z FROM p_a a, p_b b JOIN p_c c ON b.id = c.id WHERE b.w * 10 = c.z
  ORDER BY 1, 2, 3;
-- the anti-join idiom: a WHERE over the null-supplying side must not move
SELECT a.id FROM p_a a LEFT JOIN p_b b ON a.id = b.id WHERE b.w IS NULL ORDER BY 1;
-- a WHERE over the preserved side of a LEFT join still drops the null-extended row
SELECT a.id, b.w FROM p_a a LEFT JOIN p_b b ON a.id = b.id WHERE a.v = 10 ORDER BY 1;
-- an ON conjunct over the null-supplying side null-extends instead of dropping
SELECT a.id, b.w FROM p_a a LEFT JOIN p_b b ON a.id = b.id AND b.w > 250 ORDER BY 1;
-- both sides of a FULL join are null-supplying, so nothing moves
SELECT a.id, b.id FROM p_a a FULL JOIN p_b b ON a.id = b.id WHERE a.v = 30 ORDER BY 1;
-- an outer join nested inside a comma join keeps its null extension
SELECT a.id, b.w, c.z FROM p_a a LEFT JOIN p_b b ON a.id = b.id, p_c c
  WHERE a.id = 1 AND c.id = 3;
-- a correlated EXISTS reports no column bounds, so it stays above the join
SELECT a.v, b.w FROM p_a a, p_b b
  WHERE a.id = b.id AND EXISTS (SELECT 1 FROM p_c c WHERE c.id = a.id)
  ORDER BY 1;
-- a conjunct referencing no column at all also stays put
SELECT a.v, b.w FROM p_a a, p_b b WHERE a.id = b.id AND 1 = 1 ORDER BY 1;
-- NULL join keys never match
INSERT INTO p_a VALUES (NULL, 99);
INSERT INTO p_b VALUES (NULL, 999);
SELECT a.v, b.w FROM p_a a, p_b b WHERE a.id = b.id ORDER BY 1;
-- an always-false WHERE returns nothing rather than a product
SELECT count(*) FROM p_a a, p_b b WHERE false;
-- grouped queries keep their WHERE on the aggregate node, so extraction has to
-- run on that path too
SELECT a.id, count(*) FROM p_a a, p_b b WHERE a.id = b.id GROUP BY a.id ORDER BY 1;
DROP TABLE p_a;
DROP TABLE p_b;
DROP TABLE p_c;
