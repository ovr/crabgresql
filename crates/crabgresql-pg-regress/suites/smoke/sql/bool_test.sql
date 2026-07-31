--
-- BOOLEAN TESTS
-- IS [NOT] TRUE / FALSE / UNKNOWN: the standard way to collapse three-valued
-- logic to two. Unlike a comparison, these never yield NULL.
--
-- the full truth table over the three boolean values
SELECT b,
       b IS TRUE      AS is_t,
       b IS NOT TRUE  AS is_not_t,
       b IS FALSE     AS is_f,
       b IS NOT FALSE AS is_not_f
  FROM (VALUES (true), (false), (NULL::boolean)) v(b);
-- IS UNKNOWN is IS NULL on a boolean operand
SELECT b, b IS UNKNOWN AS unk, b IS NULL AS nul, b IS NOT UNKNOWN AS not_unk
  FROM (VALUES (true), (false), (NULL::boolean)) v(b);
-- the default column name is ?column?, not the clause spelling
SELECT true IS TRUE;
-- an untyped literal takes boolean from the test, so 'true'/'f' parse
SELECT 'true' IS TRUE AS lit_t, 'f' IS FALSE AS lit_f;
-- over a table column, including a NULL row
CREATE TABLE flags (name text, ok boolean);
INSERT INTO flags VALUES ('a', true), ('b', false), ('c', NULL);
SELECT name, ok IS TRUE AS yes, ok IS NOT TRUE AS not_yes FROM flags ORDER BY name;
-- in a WHERE predicate: IS NOT TRUE keeps the NULL row, <> true drops it
SELECT name FROM flags WHERE ok IS NOT TRUE ORDER BY name;
SELECT name FROM flags WHERE ok <> true ORDER BY name;
-- IS UNKNOWN in WHERE selects exactly the NULL row
SELECT name FROM flags WHERE ok IS UNKNOWN;
-- a comparison is itself a boolean, so the tests compose with one
SELECT name FROM flags WHERE (ok = true) IS NOT TRUE ORDER BY name;
-- negation and chaining still parse: IS binds tighter than NOT
SELECT NOT true IS FALSE AS chained;
-- EXPLAIN prints the spelling back; IS UNKNOWN does not collapse to IS NULL
EXPLAIN (COSTS OFF) SELECT name FROM flags WHERE ok IS NOT TRUE;
EXPLAIN (COSTS OFF) SELECT name FROM flags WHERE ok IS UNKNOWN;
-- error: the operand must be boolean
SELECT 1 IS TRUE;
SELECT 1 IS UNKNOWN;
-- error: an untyped literal that is not a valid boolean
SELECT 'a' IS TRUE;
DROP TABLE flags;
