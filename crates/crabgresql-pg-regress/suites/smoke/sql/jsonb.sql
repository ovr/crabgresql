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
-- extraction returns a canonicalized value, unlike the verbatim json operators
SELECT '{"a":{"b" :1}}'::jsonb -> 'a' AS canonical;
-- ->> yields text: a string loses its quotes, other values render canonically
-- (numeric scale is kept, but the exponent form is normalized)
SELECT '{"a":"x"}'::jsonb ->> 'a' AS str, '{"a": 1.500}'::jsonb ->> 'a' AS scale,
       '{"a": 1e2}'::jsonb ->> 'a' AS exponent;
-- a JSON null is SQL NULL through ->>
SELECT '{"a": null}'::jsonb -> 'a' AS raw, '{"a": null}'::jsonb ->> 'a' AS as_text;
-- array subscripts are 0-based and count from the end when negative
SELECT '[10,20,30]'::jsonb -> 1 AS second, '[10,20,30]'::jsonb -> -1 AS last,
       '[10,20,30]'::jsonb -> 9 AS past_end;
-- a missing key or a wrong container kind is NULL, not an error
SELECT '{"a":1}'::jsonb -> 'zz' AS missing, '1'::jsonb -> 'a' AS scalar,
       '[1,2]'::jsonb -> 'a' AS array_by_key, '{"a":1}'::jsonb -> 0 AS object_by_index;
-- #> / #>> walk a text[] path; the empty path returns the whole value
SELECT '{"a":{"b":["c","d"]}}'::jsonb #> '{a,b,1}' AS path,
       '{"a":{"b":["c","d"]}}'::jsonb #>> '{a,b,1}' AS path_text,
       '{"a":1}'::jsonb #> '{}' AS empty_path;
-- a path element indexes an array only when it parses as an integer; a NULL
-- element, or a path running past the end, makes the whole result NULL
SELECT '[1,2,3]'::jsonb #> '{-1}' AS neg_step, '[1,2]'::jsonb #> '{xx}' AS bad_step,
       '{"a":1}'::jsonb #> '{a,b}' AS too_deep,
       '{"a":1}'::jsonb #> ARRAY['a', NULL] AS null_step;
-- a NULL key propagates (the operators are strict in both arguments)
SELECT '{"a":1}'::jsonb -> NULL::text AS null_key;
-- int2 widens to the int4 subscript operator, and varchar[] to the text[] path
SELECT '[1,2]'::jsonb -> 1::smallint AS int2_ok,
       '{"a":1}'::jsonb #> ARRAY['a']::varchar[] AS varchar_path;
-- no operator for other right-hand types: the subscript operator is on integer,
-- so bigint and integer[] find no candidate
SELECT '{"a":1}'::jsonb -> 1.5;
SELECT '[1,2]'::jsonb -> 1::bigint;
SELECT '{"a":1}'::jsonb #> ARRAY[1];
-- nor for a non-json left operand
SELECT 'x'::text -> 'a';
-- with both operands untyped every candidate applies equally (42725, not 42883)
SELECT '{"a":1}' -> 'a';
