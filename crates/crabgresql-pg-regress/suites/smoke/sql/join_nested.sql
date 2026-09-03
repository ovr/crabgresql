--
-- Parenthesised join trees
-- The SQL standard lets a join expression be nested in parentheses, and
-- `pg_get_viewdef` prints every join that way. A nested group is one join
-- *input* to the chain around it, not a run of relations spliced into it.
--
CREATE TABLE pj_a (id int4, av text);
CREATE TABLE pj_b (id int4, bv text);
CREATE TABLE pj_c (id int4, cv text);
CREATE TABLE pj_d (id int4, dv text);
INSERT INTO pj_a VALUES (1,'a1'),(2,'a2'),(3,'a3');
INSERT INTO pj_b VALUES (1,'b1'),(2,'b2');
INSERT INTO pj_c VALUES (1,'c1'),(3,'c3');
INSERT INTO pj_d VALUES (1,'d1'),(3,'d3');
-- a parenthesised inner join in the right position of a LEFT JOIN. The subtree
-- is one input, so an `a` row with no match keeps its NULLs — flattening this
-- into `(a LEFT JOIN c) JOIN d` would drop row 2.
SELECT a.id, c.cv, d.dv
  FROM (pj_a a LEFT JOIN (pj_c c JOIN pj_d d ON (c.id = d.id)) ON (a.id = c.id))
 ORDER BY a.id;
-- the shape `information_schema.columns` is written in: left-deep nesting whose
-- right operands are themselves parenthesised joins.
SELECT a.id, b.bv, c.cv, d.dv
  FROM (((pj_a a
    LEFT JOIN pj_b b ON (a.id = b.id))
    LEFT JOIN (pj_c c JOIN pj_d d ON (c.id = d.id)) ON (a.id = c.id)))
 ORDER BY a.id;
-- a parenthesised group on the LEFT of a join
SELECT a.id, b.bv, c.cv
  FROM ((pj_a a LEFT JOIN pj_b b ON (a.id = b.id)) JOIN pj_c c ON (a.id = c.id))
 ORDER BY a.id;
-- USING inside a nested group, and a nested group alongside a comma group
SELECT a.id, d.dv FROM (pj_a a JOIN pj_d d USING (id)) ORDER BY a.id;
SELECT count(*) FROM pj_a x, (pj_b b JOIN pj_c c ON (b.id = c.id));
-- A nested group's merged USING column has to travel out with it: `id` appears
-- once in `*` and resolves unqualified. Republishing both physical copies would
-- show two `id` columns and make the bare name ambiguous.
SELECT * FROM (pj_a a JOIN pj_b b USING (id)) ORDER BY id;
SELECT id FROM (pj_a a JOIN pj_b b USING (id)) ORDER BY id;
-- The same when the merged group is the *right* operand: the enclosing chain
-- has merged nothing itself, so its own view has to be materialized to hold the
-- inner one. `pj_a.id` and the group's merged `id` are then two columns.
SELECT * FROM (pj_a a JOIN (pj_b b JOIN pj_c c USING (id)) ON (true))
 ORDER BY a.id, b.id LIMIT 2;
-- ... and on the left, where the merge is already in hand.
SELECT * FROM ((pj_a a JOIN pj_b b USING (id)) JOIN pj_c c ON (true))
 ORDER BY a.id, c.id LIMIT 2;
-- NATURAL against a nested merged group matches the merged column, once.
SELECT * FROM pj_a a NATURAL JOIN (pj_b b JOIN pj_c c USING (id)) ORDER BY id;
-- a qualifier is unique across the whole FROM clause, parentheses or not
SELECT 1 FROM (pj_a a JOIN pj_b a ON (true));
-- Divergence: PostgreSQL collapses an aliased join tree into one relation named
-- by the alias, hiding the inner qualifiers. Nothing here renames a whole
-- subtree yet.
SELECT 1 FROM (pj_a a JOIN pj_b b ON (true)) AS x;
-- Divergence: PostgreSQL answers this, because `x` does precede the group in
-- the FROM clause. Here the enclosing join is spliced in *above* the whole
-- subtree, so `x`'s columns are not in the row anything inside it is fed.
SELECT 1 FROM pj_a x JOIN (pj_b b JOIN LATERAL (SELECT x.id) l ON true) ON true;
DROP TABLE pj_d;
DROP TABLE pj_c;
DROP TABLE pj_b;
DROP TABLE pj_a;
