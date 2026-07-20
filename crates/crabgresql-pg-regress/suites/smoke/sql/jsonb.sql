--
-- JSONB
-- jsonb: input is parsed and canonicalized -- insignificant whitespace is
-- dropped, object keys are sorted (shorter keys first, then by byte order) with
-- duplicates collapsed keeping the last value, and numbers are normalized
-- through numeric. Covers canonical output, casts, jsonb -> scalar extraction,
-- equality/ordering, and DISTINCT. Output hand-checked against PostgreSQL.
--
-- object keys are sorted and duplicates keep the last value
SELECT '{"b": 1, "a": 2, "a": 3}'::jsonb;
-- key ordering is length-first, then byte order (shorter keys sort first)
SELECT '{"aa": 1, "b": 2}'::jsonb;
-- insignificant whitespace is removed; nesting is preserved
SELECT '  [1,   2 , ["x" ,null, true]]  '::jsonb;
-- numbers are normalized via numeric (scale is preserved, exponents expanded)
SELECT '1.0'::jsonb AS a, '1.00'::jsonb AS b, '1e2'::jsonb AS c, '-0'::jsonb AS d;
-- casts to text (canonical) and to json
SELECT '{"b":1,"a":2}'::jsonb::text AS as_text,
       '{"b":1,"a":2}'::jsonb::json AS as_json;
-- jsonb scalar -> SQL scalar extraction
SELECT 'true'::jsonb::bool AS b,
       '42'::jsonb::int4 AS i,
       '3.5'::jsonb::numeric AS n;
-- extracting a scalar of the wrong kind is an error (22023)
SELECT '"x"'::jsonb::numeric;
SELECT '[1]'::jsonb::bool;
-- malformed input: after a comma a key is required (its DETAIL says
-- "Expected string", not "or }"); a too-large number is a 22003 error
SELECT '{"a":1,}'::jsonb;
SELECT '1e1000000'::jsonb;
-- equality is value-based: 1.0 and 1.00 are equal, and whitespace is irrelevant
SELECT '[1, 2]'::jsonb = '[1,2]'::jsonb AS eq_ws,
       '1.0'::jsonb = '1.00'::jsonb AS eq_num,
       '{"a":1}'::jsonb = '{"a":2}'::jsonb AS neq;
-- ordering follows PG's rule: Null < String < Number < Bool < Array < Object,
-- with arrays/objects compared by length first
SELECT j FROM (VALUES
    ('{"a": 1}'::jsonb),
    ('[1, 2, 3]'::jsonb),
    ('[5]'::jsonb),
    ('42'::jsonb),
    ('"hello"'::jsonb),
    ('true'::jsonb),
    ('null'::jsonb)
) AS t(j) ORDER BY j;
-- round-trip through a table column, plus GROUP BY (which hashes jsonb):
-- rows that are jsonb-equal despite different spelling collapse into one group
CREATE TABLE btest (id int, doc jsonb);
INSERT INTO btest VALUES
    (1, '{"a": 1, "b": 2}'),
    (2, '{"b":2,"a":1}'),
    (3, '[1]'),
    (4, '{"x":  [1,2,3], "k": "v"}');
SELECT id, doc FROM btest ORDER BY id;
SELECT doc, count(*) AS n FROM btest GROUP BY doc ORDER BY doc;
DROP TABLE btest;
