--
-- JOIN
-- Multiple FROM items and CROSS JOIN: cartesian products. Column resolution
-- across relations, qualified wildcards, and the ambiguity / duplicate-alias
-- errors PG raises.
--
-- comma-separated FROM items form the cartesian product; the first relation is
-- outermost, the last varies fastest
SELECT * FROM (VALUES (1), (2)) a(x), (VALUES (10), (20)) b(y);
-- explicit CROSS JOIN is the same cartesian product
SELECT a.x, b.y FROM (VALUES (1)) a(x) CROSS JOIN (VALUES (7), (8)) b(y);
-- three-way product
SELECT * FROM (VALUES (1), (2)) a(x), (VALUES (3)) b(y), (VALUES (4), (5)) c(z);
-- real tables with a join predicate in WHERE
CREATE TABLE t1 (id integer, label text);
INSERT INTO t1 VALUES (1, 'one'), (2, 'two');
CREATE TABLE t2 (id integer, tag text);
INSERT INTO t2 VALUES (1, 'a'), (2, 'b');
SELECT t1.label, t2.tag FROM t1, t2 WHERE t1.id = t2.id ORDER BY 1;
-- a qualified wildcard selects just that relation's columns
SELECT t2.* FROM t1, t2 WHERE t1.id = t2.id AND t1.id = 1;
-- an unqualified column present in both relations is ambiguous
SELECT id FROM t1, t2;
-- a qualified reference to a missing column names the qualifier
SELECT t1.nope FROM t1, t2;
-- a duplicate column alias makes a reference ambiguous
SELECT x FROM (VALUES (1, 2)) v(x, x);
-- the same qualifier twice is rejected
SELECT * FROM t1, t1;
-- a column-list alias renames a base table's columns positionally
SELECT c1, c2 FROM t1 AS r(c1, c2) ORDER BY c1;
-- fewer aliases than columns rename only the leading columns; the rest keep
-- their original names
SELECT * FROM t1 AS r(c1) ORDER BY 1;
-- the renamed columns drive WHERE and the projection
SELECT c2 FROM t1 AS r(c1, c2) WHERE c1 = 2;
-- more column aliases than the table has columns is an error
SELECT * FROM t1 AS r(a, b, c);

-- explicit INNER JOIN ... ON preserves duplicate matches and does not match
-- NULL join keys
SELECT a.x, b.y
FROM (VALUES (1), (2), (NULL)) a(x)
INNER JOIN (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y
ORDER BY a.x NULLS LAST, b.y NULLS LAST;
-- LEFT JOIN null-extends unmatched rows from the left input
SELECT a.x, b.y
FROM (VALUES (1), (2), (NULL)) a(x)
LEFT JOIN (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y
ORDER BY a.x NULLS LAST, b.y NULLS LAST;
-- RIGHT OUTER JOIN null-extends unmatched rows from the right input
SELECT a.x, b.y
FROM (VALUES (1), (2), (NULL)) a(x)
RIGHT OUTER JOIN (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y
ORDER BY a.x NULLS LAST, b.y NULLS LAST;
-- FULL OUTER JOIN preserves unmatched rows from both inputs
SELECT a.x, b.y
FROM (VALUES (1), (2), (NULL)) a(x)
FULL OUTER JOIN (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y
ORDER BY a.x NULLS LAST, b.y NULLS LAST;
-- a later join in the chain sees the null-extended output of the prior join
SELECT a.x, b.y, c.z
FROM (VALUES (1)) a(x)
LEFT JOIN (VALUES (9)) b(y) ON false
JOIN (VALUES (2)) c(z) ON b.y IS NULL
ORDER BY 1, 3;
-- joins are valid aggregate inputs; count(expr) ignores null-extended values
SELECT count(*), count(b.y)
FROM (VALUES (1), (2), (NULL)) a(x)
LEFT JOIN (VALUES (2), (2), (3), (NULL)) b(y) ON a.x = b.y;
-- end ON-join coverage

-- JOIN ... USING merges the join column: it appears once, first, then each
-- input's remaining columns. On base tables:
SELECT * FROM t1 JOIN t2 USING (id) ORDER BY id;
-- the merged column resolves unqualified and each side stays addressable
SELECT id, a.id, b.id, a.x, b.y
FROM (VALUES (1, 10)) a(id, x)
JOIN (VALUES (1, 100)) b(id, y) USING (id);
-- a multi-column USING equates every listed column
SELECT *
FROM (VALUES (1, 1, 'p'), (1, 2, 'q')) a(g, h, v)
JOIN (VALUES (1, 1, 'z')) b(g, h, w) USING (g, h)
ORDER BY g, h;
-- LEFT JOIN USING null-extends the right side; the merged column keeps the
-- left value on unmatched rows
SELECT *
FROM (VALUES (1), (2)) a(id)
LEFT JOIN (VALUES (1, 100), (3, 300)) b(id, y) USING (id)
ORDER BY id;
-- RIGHT JOIN USING: the merged column takes the right value
SELECT *
FROM (VALUES (1, 10)) a(id, x)
RIGHT JOIN (VALUES (1), (3)) b(id) USING (id)
ORDER BY id;
-- FULL JOIN USING: the merged column is COALESCE(left, right), so it is never
-- the null-extended side
SELECT *
FROM (VALUES (1, 10), (2, 20)) a(id, x)
FULL JOIN (VALUES (1, 100), (3, 300)) b(id, y) USING (id)
ORDER BY id;
-- NATURAL JOIN derives USING from every common column name
SELECT *
FROM (VALUES (1, 'a'), (2, 'b')) a(id, label)
NATURAL JOIN (VALUES (1, 'a'), (1, 'x')) b(id, label)
ORDER BY id, label;
-- NATURAL JOIN with no common column name is a cross product
SELECT *
FROM (VALUES (1), (2)) a(x)
NATURAL JOIN (VALUES (10), (20)) b(y)
ORDER BY x, y;
-- a USING join in a later comma group resolves against the shifted combined row
SELECT *
FROM (VALUES (1)) t0(k),
     (VALUES (5, 50)) a(id, x) JOIN (VALUES (5, 500)) b(id, y) USING (id)
ORDER BY k;
-- a USING column absent from the right input names the right table
SELECT * FROM t1 JOIN t2 USING (label);
-- a USING column absent from the left input names the left table
SELECT * FROM t1 JOIN t2 USING (tag);
-- a USING column that is ambiguous within one input is rejected
SELECT * FROM (VALUES (1, 2)) a(x, x) JOIN (VALUES (1)) b(x) USING (x);
-- a column named twice in the same USING list is rejected
SELECT * FROM t1 JOIN t2 USING (id, id);
-- end USING/NATURAL coverage
