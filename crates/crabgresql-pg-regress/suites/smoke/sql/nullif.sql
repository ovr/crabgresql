--
-- NULLIF
-- The standard's `CASE WHEN a = b THEN NULL ELSE a END` shorthand: comparison
-- through the `=` operator, result type from the compared operands.
--
-- equal arguments yield NULL (renders empty), unequal ones the first argument;
-- the default column name is "nullif"
SELECT NULLIF(1, 1) AS equal, NULLIF(1, 2) AS unequal;
SELECT NULLIF('a', 'b');
-- NULL is never "equal", so a NULL first argument stays NULL and a NULL second
-- argument returns the first
SELECT NULLIF(NULL::int, 1) AS left_null, NULLIF(1, NULL) AS right_null;
-- the result type is the type the `=` operator resolved its operands to, which
-- is why a mixed comparison reports the promoted type
SELECT pg_typeof(NULLIF(1::int, 2.5)) AS ty, pg_typeof(NULLIF('a', 'b')) AS ty;
-- the classic use: turn a sentinel into NULL, here to divide safely
CREATE TABLE quotas (team text, allowed integer);
INSERT INTO quotas VALUES ('a', 4), ('b', 0), ('c', 2);
SELECT team, 100 / NULLIF(allowed, 0) AS per_unit FROM quotas ORDER BY team;
-- in a WHERE predicate: the NULL row is not returned, as with any NULL test
SELECT team FROM quotas WHERE NULLIF(allowed, 0) IS NULL;
-- error: an untyped argument that does not fit the compared type
SELECT NULLIF(1, 'x');
-- error: the two arguments must be comparable. Known gap: this is the shared
-- wording every failed operator resolution uses here, where PG prints one HINT
-- ("No operator matches the given name and argument types. …") and a cursor.
SELECT NULLIF(1, true);
-- error: exactly two arguments. PG rejects both of these in the grammar and
-- prints the same message, with a cursor under the offending token
SELECT NULLIF(1);
SELECT NULLIF(1, 2, 3);
DROP TABLE quotas;
