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
