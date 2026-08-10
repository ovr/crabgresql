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
-- the result is the left argument as the `=` operator takes it. PG compares these
-- pairs cross-type, so the left argument is not coerced and keeps its own type --
-- which is what keeps a real from being printed at double precision and a
-- timestamp from picking up a zone
SELECT pg_typeof(NULLIF(1::int2, 1::int8)) AS int2_int8,
       pg_typeof(NULLIF(1::float4, 1::float8)) AS float4_float8,
       NULLIF(0.1::float4, 1::float8) AS float4_value;
SELECT pg_typeof(NULLIF('2020-01-01'::date, '2020-01-01'::timestamp)) AS date_ts,
       NULLIF('2020-01-01 05:00'::timestamp, '2020-06-01'::timestamptz) AS ts_tstz,
       pg_typeof(NULLIF('a'::name, 'a'::text)) AS name_text;
-- where PG has no cross-type operator it coerces both sides first, and then the
-- comparison's own operand type is the result: int against numeric compares in
-- numeric, and varchar compares as text (PG has no varchar `=`)
SELECT pg_typeof(NULLIF(1::int, 2.5)) AS int_numeric, pg_typeof(NULLIF('a', 'b')) AS unknowns,
       pg_typeof(NULLIF('a'::varchar, 'b'::varchar)) AS varchars;
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
-- error: like COALESCE, NULLIF is a keyword and takes no function decorations, and
-- is not reachable under quotes (see coalesce.sql for the cursor/token-case and
-- HINT gaps these share)
SELECT NULLIF(1, 2) OVER ();
SELECT "nullif"(1, 2);
DROP TABLE quotas;
