--
-- Correlated subqueries
-- A subquery that references a column of the enclosing query is re-evaluated
-- per outer row (the outer references filled from that row) — the shapes TPC-H
-- Q2/Q4/Q17/Q20/Q21/Q22 depend on.
--
CREATE TABLE cq1 (a integer, b integer);
INSERT INTO cq1 VALUES (1, 10), (2, 20), (3, 30);
CREATE TABLE cq2 (a integer, c integer);
INSERT INTO cq2 VALUES (1, 100), (1, 200), (2, 20), (2, 50), (4, 400);
-- correlated EXISTS: keep outer rows with a matching cq2 row
SELECT a FROM cq1 WHERE EXISTS (SELECT 1 FROM cq2 WHERE cq2.a = cq1.a) ORDER BY a;
-- correlated NOT EXISTS: the anti-join complement
SELECT a FROM cq1 WHERE NOT EXISTS (SELECT 1 FROM cq2 WHERE cq2.a = cq1.a) ORDER BY a;
-- correlated scalar subquery in the target list; no matching row -> NULL
SELECT a, (SELECT max(c) FROM cq2 WHERE cq2.a = cq1.a) AS mc FROM cq1 ORDER BY a;
-- correlated scalar-aggregate comparison in WHERE; a=3's subquery is empty
-- (NULL), so three-valued logic drops the row
SELECT a FROM cq1 WHERE b < (SELECT max(c) FROM cq2 WHERE cq2.a = cq1.a) ORDER BY a;
-- correlated IN: the candidate set depends on the outer row
SELECT a FROM cq1 WHERE b IN (SELECT c FROM cq2 WHERE cq2.a = cq1.a) ORDER BY a;
-- unqualified outer reference: b resolves outward (cq2 has no b)
SELECT a FROM cq1 WHERE EXISTS (SELECT 1 FROM cq2 WHERE c = b) ORDER BY a;
-- correlation from a join outer: the outer reference indexes the joined row
SELECT cq1.a FROM cq1 JOIN cq2 ON cq1.a = cq2.a
  WHERE cq1.b < (SELECT max(c) FROM cq2 z WHERE z.a = cq1.a) ORDER BY cq1.a;
-- two-level correlation: the inner EXISTS references both the middle (y.c) and
-- the outermost (x.a) query
SELECT a FROM cq1 x WHERE EXISTS (
  SELECT 1 FROM cq2 y WHERE y.a = x.a AND EXISTS (
    SELECT 1 FROM cq1 z WHERE z.a = x.a AND z.b = y.c)) ORDER BY a;
