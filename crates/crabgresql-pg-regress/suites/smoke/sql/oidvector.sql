--
-- OIDVECTOR / INT2VECTOR
-- The two fixed-element-type vectors PostgreSQL uses in its own catalogs
-- (pg_proc.proargtypes, pg_index.indkey/indoption,
-- pg_partitioned_table.partattrs): input parsing, space-separated output,
-- 0-based subscripting, unnest, ordering, and storage.
--
-- The two types look alike but share almost no rules. Every asymmetry below
-- was derived by probing PostgreSQL 18.4:
--   * oidvector scans each element like C's strtoul(s, &end, 0), so hex and
--     octal spellings convert; int2vector is decimal-only.
--   * oidvector has no whitespace-delimited "token" -- each scan resumes where
--     the last stopped, so '08' is TWO elements; int2vector is token-based, so
--     '08' is the one element 8.
--   * oidvector separates on C's isspace (tab, vertical tab, ...);
--     int2vector separates on the space character alone.
--   * oidvector orders by element COUNT first (it has its own btoidvectorcmp
--     opclass); int2vector has none and orders element-wise.
-- Both quote a rejection through to the end of the whole input, and name the
-- *element* type -- oid or smallint, never oidvector.
--
-- The pg_input_is_valid / pg_input_error_info cases are ported from
-- vendor/postgres/regress/sql/oid.sql; the array-construction cases from
-- vendor/postgres/regress/sql/arrays.sql. The rest of both upstream files is
-- unrelated, so neither is on the promotion list in
-- suites/upstream_must_pass.txt.
--
-- Everything below matches PostgreSQL 18.4 byte for byte except the two
-- `oid[]` conversions at the end, both recorded in the expected output:
--   * `oidvector::oid[]` is accepted by PostgreSQL and yields a *0-based*
--     array, printing as `[0:1]={1,2}`. This build's array type has no
--     lower-bound concept, so rendering that as `{1,2}` would be a silently
--     different value; the cast is rejected instead.
--   * `oid[]::oidvector` is rejected by both, with the same message and
--     SQLSTATE, but this build omits PostgreSQL's `LINE 1: ... ^` caret --
--     `cannot cast type ...` is built without a span. That gap is shared by
--     every cast-context error here, not specific to these types.
--

-- Input: whitespace runs separate elements and are trimmed; empty is legal.
SELECT ' 1 2  4 '::oidvector;
SELECT ''::oidvector;
SELECT ' 1  2   3 '::int2vector;
SELECT ''::int2vector;

-- oidvector elements scan base-0, so hex/octal convert and a negative wraps
-- into the unsigned range, exactly as `oid` itself does.
SELECT '1 0x1f 010'::oidvector;
SELECT '-1'::oidvector;
SELECT '-2147483648 18446744073709551615'::oidvector;

-- oidvector resumes each scan where the last one stopped, so a trailing
-- character that itself converts simply begins the next element.
SELECT '08'::oidvector;
SELECT '1 08 9'::oidvector;
SELECT '1-2'::oidvector;
SELECT '12+3'::oidvector;

-- int2vector is decimal-only, signed, and token-based: `010` is ten, not
-- eight, and `08` is one element rather than two.
SELECT '1 010'::int2vector;
SELECT '08'::int2vector;
SELECT '+5 -0 -32768 32767'::int2vector;
SELECT '0x10'::int2vector;                      -- error
SELECT '1-2'::int2vector;                       -- error

-- Separator sets: oidvector takes any C isspace, int2vector only the space.
SELECT E'7\t8'::oidvector, E'7\x0b8'::oidvector, E'7\r8'::oidvector;
SELECT E'7\t8'::int2vector;                     -- error

-- Malformed elements name the element type, not the vector type.
SELECT '01 01XYZ'::oidvector;                   -- error
SELECT '1 34junk 9'::oidvector;                 -- error
SELECT '1 ,2 3'::oidvector;                     -- error
SELECT '1 5x 7'::int2vector;                    -- error

-- Out-of-range elements are 22003, quoted from the element's first character.
SELECT '01 9999999999'::oidvector;              -- error
SELECT '1 -32769'::int2vector;                  -- error

-- Soft input validation (ported from upstream oid.sql).
SELECT pg_input_is_valid(' 1 2  4 ', 'oidvector');
SELECT pg_input_is_valid('01 01XYZ', 'oidvector');
SELECT * FROM pg_input_error_info('01 01XYZ', 'oidvector');
SELECT pg_input_is_valid('01 9999999999', 'oidvector');
SELECT * FROM pg_input_error_info('01 9999999999', 'oidvector');
SELECT * FROM pg_input_error_info('1 5x 7', 'int2vector');

-- Subscripting is 0-based, unlike an array; out of range is NULL, not an error.
SELECT ('11 22 33'::oidvector)[0];
SELECT ('11 22 33'::oidvector)[2];
SELECT ('11 22 33'::oidvector)[3] IS NULL;
SELECT ('11 22 33'::oidvector)[-1] IS NULL;

-- A NULL base still evaluates the subscript expression, so an error there is
-- raised rather than short-circuited away to NULL. Covers arrays too, since
-- both share the Subscript evaluation.
SELECT (NULL::oidvector)[1/0];                  -- error
SELECT (NULL::int[])[1/0];                      -- error
SELECT (NULL::oidvector)[0] IS NULL;

-- unnest expands to the element type.
SELECT unnest('11 22 33'::oidvector);
SELECT unnest('11 22 33'::int2vector);

-- Ordering. At equal length both kinds compare element-wise, and a common
-- prefix puts the shorter one first.
SELECT '1 2'::oidvector = '1 2'::oidvector;
SELECT '1 2'::oidvector < '1 3'::oidvector;
SELECT '1'::oidvector < '1 2'::oidvector;
SELECT '2 0'::oidvector < '1 9'::oidvector;

-- At unequal length the two kinds disagree: oidvector compares the element
-- count first, int2vector compares elements first.
SELECT '2'::oidvector < '1 1'::oidvector;
SELECT '2'::int2vector < '1 1'::int2vector;
SELECT v FROM (VALUES ('9 8'::oidvector), ('1 1 1'::oidvector), ('7'::oidvector)) t(v)
  ORDER BY v;
SELECT v FROM (VALUES ('9 8'::int2vector), ('1 1 1'::int2vector), ('7'::int2vector)) t(v)
  ORDER BY v;

-- Treated as a scalar for array construction: this is an array *of vectors*,
-- not a flattened oid[] (ported from upstream arrays.sql).
SELECT array['11 22 33'::oidvector];
SELECT array['11 22 33'::int2vector];

-- Neither conversion to oid[] is available; see the header.
SELECT '{1,2}'::oid[]::oidvector;               -- error
SELECT '1 2'::oidvector::oid[];                 -- error

-- Storage, ordering and dedup through a real table column.
CREATE TABLE vectbl (id int, v oidvector, w int2vector);
INSERT INTO vectbl VALUES (1, '11 22 33', '1 2'), (2, '11 22', '3'), (3, '11 22 33', '1 2');
SELECT id, v, w FROM vectbl ORDER BY id;
SELECT v FROM vectbl ORDER BY v, id;
SELECT DISTINCT v FROM vectbl ORDER BY v;
SELECT v, count(*) FROM vectbl GROUP BY v ORDER BY v;
DROP TABLE vectbl;

-- The catalog columns that are really vectors.
CREATE TABLE vecidx (a int, b int, PRIMARY KEY (b, a));
SELECT indkey, indoption FROM pg_index i
  JOIN pg_class c ON c.oid = i.indrelid WHERE c.relname = 'vecidx';
DROP TABLE vecidx;
