--
-- PG_LSN
-- WAL log sequence numbers: input parsing, the `X/YYYYYYYY` output, ordering,
-- and the arithmetic against `numeric` with its three failure modes.
--
-- Ported from vendor/postgres/regress/sql/pg_lsn.sql. Every `pg_lsn`-specific
-- result there is reproduced exactly; upstream `pg_lsn` is nonetheless not on
-- the promotion list in suites/upstream_must_pass.txt, because two things
-- unrelated to this type still block it:
--   1. `pg_lsn '0/16AE7F7'` -- the parser rejects the `type 'literal'` spelling
--      for any bareword type name (only `xml` is excepted).
--   2. `EXPLAIN (COSTS OFF)`, whose plan text this planner does not reproduce.
-- Both are dropped below.
--
-- NOTE ON OUTPUT FORMAT: the low half is zero-padded to eight hex digits
-- (`0/00000000`), which is what the vendored 19devel corpus expects and what
-- the version this server advertises does. PostgreSQL 18 prints it unpadded.
--

CREATE TABLE PG_LSN_TBL (f1 pg_lsn);

-- Largest and smallest input
INSERT INTO PG_LSN_TBL VALUES ('0/0');
INSERT INTO PG_LSN_TBL VALUES ('FFFFFFFF/FFFFFFFF');

-- Incorrect input
INSERT INTO PG_LSN_TBL VALUES ('G/0');
INSERT INTO PG_LSN_TBL VALUES ('-1/0');
INSERT INTO PG_LSN_TBL VALUES (' 0/12345678');
INSERT INTO PG_LSN_TBL VALUES ('ABCD/');
INSERT INTO PG_LSN_TBL VALUES ('/ABCD');
-- a second slash lands in the low half, and neither half takes nine digits
INSERT INTO PG_LSN_TBL VALUES ('0/1/2');
INSERT INTO PG_LSN_TBL VALUES ('000000000/1');

-- Also try it with non-error-throwing API
SELECT pg_input_is_valid('16AE7F7', 'pg_lsn');
SELECT * FROM pg_input_error_info('16AE7F7', 'pg_lsn');

-- Min/Max aggregation
SELECT MIN(f1), MAX(f1) FROM PG_LSN_TBL;

DROP TABLE PG_LSN_TBL;

-- input is case-insensitive; output is uppercase with the low half padded
SELECT 'abcd/ef'::pg_lsn, 'ABCD/EF'::pg_lsn;

-- Operators
SELECT '0/16AE7F8' = '0/16AE7F8'::pg_lsn;
SELECT '0/16AE7F8'::pg_lsn != '0/16AE7F7';
SELECT '0/16AE7F7' < '0/16AE7F8'::pg_lsn;
SELECT '0/16AE7F8' > '0/16AE7F7'::pg_lsn;
SELECT '0/16AE7F7'::pg_lsn - '0/16AE7F8'::pg_lsn;
SELECT '0/16AE7F8'::pg_lsn - '0/16AE7F7'::pg_lsn;
SELECT '0/16AE7F7'::pg_lsn + 16::numeric;
SELECT 16::numeric + '0/16AE7F7'::pg_lsn;
SELECT '0/16AE7F7'::pg_lsn - 16::numeric;
-- an untyped or integer operand takes numeric, so no explicit cast is needed
SELECT '0/16AE7F7'::pg_lsn + 16;
SELECT '0/16AE7F7'::pg_lsn + 16::int;
-- a fractional operand rounds half away from zero
SELECT '0/16AE7F7'::pg_lsn + 1.5, '0/16AE7F7'::pg_lsn + 1.7;
SELECT 'FFFFFFFF/FFFFFFFE'::pg_lsn + 1::numeric;
SELECT 'FFFFFFFF/FFFFFFFE'::pg_lsn + 2::numeric; -- out of range error
SELECT '0/1'::pg_lsn - 1::numeric;
SELECT '0/1'::pg_lsn - 2::numeric; -- out of range error
SELECT '0/0'::pg_lsn + ('FFFFFFFF/FFFFFFFF'::pg_lsn - '0/0'::pg_lsn);
SELECT 'FFFFFFFF/FFFFFFFF'::pg_lsn - ('FFFFFFFF/FFFFFFFF'::pg_lsn - '0/0'::pg_lsn);
SELECT '0/16AE7F7'::pg_lsn + 'NaN'::numeric;
SELECT '0/16AE7F7'::pg_lsn - 'NaN'::numeric;
SELECT '0/16AE7F7'::pg_lsn + 'Infinity'::numeric;

-- an untyped literal resolves per operator: to pg_lsn under `-` (the only
-- operator with a pg_lsn on both sides) and to numeric under `+`
SELECT '0/16AE7F8'::pg_lsn - '0/16AE7F7';
SELECT '0/16AE7F8' - '0/16AE7F7'::pg_lsn;
SELECT '0/1'::pg_lsn + '16';
SELECT '16' + '0/1'::pg_lsn;

-- float8 -> numeric is an assignment cast, not an implicit one, so there is no
-- operator for a float operand
SELECT '0/16AE7F7'::pg_lsn + 1.5::float8;
SELECT '0/16AE7F7'::pg_lsn - 1.5::float8;

-- an operand too wide for the internal accumulator is out of range, not a crash
SELECT '0/1'::pg_lsn + 170141183460469231731687303715884105727;
SELECT '0/2'::pg_lsn - (-170141183460469231731687303715884105728);

-- the combinations with no operator at all
SELECT '0/1'::pg_lsn * '0/1'::pg_lsn;
SELECT '0/1'::pg_lsn / '0/1'::pg_lsn;
SELECT 16::numeric - '0/1'::pg_lsn;

-- pg_lsn(numeric)
SELECT pg_lsn(23783416::numeric);
SELECT pg_lsn(0::numeric);
SELECT pg_lsn(18446744073709551615::numeric);
SELECT pg_lsn(-1::numeric);
SELECT pg_lsn(18446744073709551616::numeric);
SELECT pg_lsn('NaN'::numeric);

-- ordering, grouping and indexing all work: pg_lsn is a plain ordered counter
CREATE TABLE lsn_tbl (l pg_lsn);
INSERT INTO lsn_tbl VALUES ('0/2'), ('0/1'), ('1/0'), ('0/1');
SELECT l FROM lsn_tbl ORDER BY l;
SELECT l, count(*) FROM lsn_tbl GROUP BY l ORDER BY l;
SELECT DISTINCT l FROM lsn_tbl ORDER BY l;
-- the index is physical, not metadata-only: this must plan as an Index Scan
CREATE INDEX lsn_tbl_ix ON lsn_tbl(l);
EXPLAIN SELECT l FROM lsn_tbl WHERE l = '0/2'::pg_lsn;
SELECT l FROM lsn_tbl WHERE l = '0/2'::pg_lsn;
SELECT l::text FROM lsn_tbl ORDER BY 1;
SELECT ARRAY['0/1'::pg_lsn, '2/3'::pg_lsn] AS arr;
DROP TABLE lsn_tbl;

-- Check btree and hash opclasses (upstream's generate_series form: each FROM
-- item's alias names its single output column)
SELECT DISTINCT (i || '/' || j)::pg_lsn f
  FROM generate_series(1, 10) i,
       generate_series(1, 10) j,
       generate_series(1, 5) k
  WHERE i <= 10 AND j > 0 AND j <= 10
  ORDER BY f;
