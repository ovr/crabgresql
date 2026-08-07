--
-- INTEGER LITERALS AND BITWISE OPERATORS
--
-- The spellings a numeric literal can take (`0x`/`0o`/`0b`, `_` separators) and
-- the integer bitwise/shift operators. Upstream covers the *quoted* forms in
-- int2/int4/int8 and the bare forms in numerology, but numerology is far from
-- passing for unrelated reasons (PREPARE, the `float8()`/`int2()` constructors),
-- so the cases below are the ones that would otherwise regress unnoticed.
--
-- Every expected value here was taken from PostgreSQL 18.4. Two error *reports*
-- diverge, both pre-existing and engine-wide rather than specific to these
-- statements (the same split is already pinned in jsonb.out):
--
--   * PostgreSQL puts "No operator matches ..." / "Could not choose a best
--     candidate operator." in a single HINT; this engine splits the sentence
--     into a DETAIL plus a shorter HINT;
--   * the 42725 "operator is not unique" report carries no caret here, because
--     the resolver that raises it is not given the operator's span.
--
-- The values, SQLSTATEs and primary messages all match.
--

-- A literal keeps its written spelling all the way to the binder, so each
-- consumer of that text has to decode it the same way. These are the places
-- that used to re-read it with a plain decimal parse: an ORDER BY / GROUP BY
-- ordinal (which failed *silently*, dropping the sort), LIMIT / OFFSET, a type
-- modifier, and a cursor count.
CREATE TABLE int_literal_tbl (a int, b int);
INSERT INTO int_literal_tbl VALUES (3, 1), (1, 2), (2, 3);

SELECT b, a FROM int_literal_tbl ORDER BY 0x2;
SELECT a FROM int_literal_tbl GROUP BY 0x1 ORDER BY 1;
SELECT a FROM int_literal_tbl ORDER BY a LIMIT 0x2;
SELECT a FROM int_literal_tbl ORDER BY a OFFSET 0b1;
SELECT '1'::varchar(0x5);
SELECT length('abcdefg'::varchar(0b101));
SELECT 1.005::numeric(0x5,0x2);

BEGIN;
DECLARE c CURSOR FOR SELECT a FROM int_literal_tbl ORDER BY a;
FETCH 0x2 FROM c;
MOVE 0b1 IN c;
COMMIT;

-- Past int8 a constant widens into numeric, and PG puts no ceiling on that.
SELECT 0x8000000000000000;
SELECT pg_typeof(0x8000000000000000);
SELECT 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF;

-- A separator sits between two digits *of the literal's own base*, so `e` does
-- not close one (`1_e5` is junk, not 100000) and neither does a `2` in binary.
SELECT 1_000, 0xE_FF, 0o2_73, 0b_10_0101;
SELECT 1_e5;
SELECT 2_e+3;
SELECT 0b1_2;
SELECT 100_;
SELECT 123abc;

-- An empty digit run names its base, and the caret sits on the literal's first
-- character rather than on the character that ended it.
SELECT 0x;
SELECT 0o;
SELECT 0b;
SELECT 0x0y;

-- Whitespace around an integer is C's `isspace`, which includes vertical tab
-- (Rust's `is_ascii_whitespace` is the one predicate that omits it) but not
-- NBSP, which `char::is_whitespace` would wrongly accept.
SELECT (chr(11) || '42')::int4;
SELECT (chr(160) || '42')::int4;

-- Integer shifts and bitwise operators. The shift count is int4 at every width,
-- so int2 widens into it but int8 matches no operator at all; PG applies no
-- overflow check to a shift, which is why `(-1)::int2 << 15` is INT16_MIN.
SELECT (-1::int2<<15)::text, 1::int8 << 2, 1 << 2::int2;
SELECT 5 & 3, 5 | 3, 5 # 3, ~5;
SELECT 1 << 2::int8;

-- An untyped literal resolves against the other operand. The two families
-- differ: `&` has two same-typed operands in every candidate, so an unknown
-- borrows the other side's width outright, while the shifts are pinned only by
-- an exact int4 count.
SELECT '1' << 2, '1' & 2, '5' & 1::int8, 1 & '3';
SELECT '1' << 2::int2;
SELECT 'a' << 'b';

-- The spellings these operators share with other families still reach them.
SELECT '10.0.0.1'::inet << '10.0.0.0/8'::inet;
SELECT B'1000' << 1;
SELECT X'42F';

DROP TABLE int_literal_tbl;
