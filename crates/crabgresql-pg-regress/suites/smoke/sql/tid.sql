--
-- TID
-- The `tid` type: input parsing (the accepted `(block,offset)` spellings and
-- the field-range rules), text output, the `tid_block`/`tid_offset`
-- accessors, ordering/grouping, arrays, and the invalid-input errors.
--
-- The first half is ported from vendor/postgres/regress/sql/tid.sql; the rest
-- of that file needs `ctid`, `currtid2()`, materialized views and sequences, so
-- upstream `tid` is not on the promotion list in suites/upstream_must_pass.txt.
--

-- input: both fields decimal; a negative block wraps into the 32-bit field,
-- and the maxima of both fields round-trip
SELECT
  '(0,0)'::tid as tid00,
  '(0,1)'::tid as tid01,
  '(-1,0)'::tid as tidm10,
  '(4294967295,65535)'::tid as tidmax;

-- a block past the field width is rejected, not truncated
SELECT '(4294967296,1)'::tid;  -- error
-- and an offset past its own, narrower field width likewise
SELECT '(1,65536)'::tid;  -- error

-- Also try it with non-error-throwing API
SELECT pg_input_is_valid('(0)', 'tid');
SELECT * FROM pg_input_error_info('(0)', 'tid');
SELECT pg_input_is_valid('(0,-1)', 'tid');
SELECT * FROM pg_input_error_info('(0,-1)', 'tid');

-- tests for tid_block() and tid_offset()
SELECT tid_block('(0,0)'::tid), tid_offset('(0,0)'::tid);
SELECT tid_block('(0,1)'::tid), tid_offset('(0,1)'::tid);
SELECT tid_block('(42,7)'::tid), tid_offset('(42,7)'::tid);
-- max values: blockno uint32 max, offset uint16 max
SELECT tid_block('(4294967295,65535)'::tid), tid_offset('(4294967295,65535)'::tid);
-- (-1,0) wraps to blockno 4294967295
SELECT tid_block('(-1,0)'::tid);
-- NULL handling (strict functions)
SELECT tid_block(NULL::tid), tid_offset(NULL::tid);
-- round-trip: blockno + offset reconstruct the original TID
SELECT t, tid_block(t), tid_offset(t),
       format('(%s,%s)', tid_block(t), tid_offset(t))::tid = t AS roundtrip_ok
FROM (VALUES ('(0,0)'::tid), ('(1,42)'::tid), ('(4294967295,65535)'::tid)) AS v(t);

-- input leniency: text outside the parens is ignored, and whitespace *leading*
-- a field is skipped -- but whitespace trailing one is not
SELECT '(0,1)x'::tid, 'x(0,1)'::tid, ' (0,1)'::tid, '(0,  1)'::tid, '(+1,+2)'::tid;
SELECT '(0 ,1)'::tid;  -- error

-- the block field takes anything that fits int4 or uint4, negatives wrapping;
-- the two spellings of a wrapped value agree
SELECT '(-2,0)'::tid, '(18446744073709551614,0)'::tid, '(-2147483648,0)'::tid;
SELECT '(-2147483649,0)'::tid;  -- error
SELECT '(99999999999999999999999,0)'::tid;  -- error

-- more input rejections: a missing field, an unparenthesized pair, junk
SELECT ''::tid;  -- error
SELECT '0,1'::tid;  -- error
SELECT '(0,1,2)'::tid;  -- error
SELECT '(0x1,1)'::tid;  -- error

-- ordering is (block, offset), which is also the heap's own row order
CREATE TABLE tid_tbl (t tid);
INSERT INTO tid_tbl VALUES ('(2,3)'), ('(1,42)'), ('(2,1)'), ('(1,42)');
SELECT t FROM tid_tbl ORDER BY t;
SELECT t, count(*) FROM tid_tbl GROUP BY t ORDER BY t;
SELECT DISTINCT t FROM tid_tbl ORDER BY t;
SELECT min(t), max(t) FROM tid_tbl;

-- tid has a default btree opclass, so it may key an index
CREATE INDEX tid_tbl_ix ON tid_tbl(t);

-- output through the generic tid -> text cast, and as an array element
SELECT t::text FROM tid_tbl ORDER BY 1;
SELECT ARRAY['(0,1)'::tid,'(2,3)'::tid] AS arr;

DROP TABLE tid_tbl;
