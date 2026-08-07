--
-- JSON
-- json: input is validated but the original text is preserved verbatim
-- (whitespace, key order, and duplicate keys are all kept). Covers casts to
-- text and to jsonb and a round-trip through a table column. Output
-- hand-checked against PostgreSQL's aligned format.
--
-- a json value prints exactly as written (whitespace and key order preserved)
SELECT '{"b": 1, "a": 2}'::json;
-- duplicate keys are preserved by json (unlike jsonb, which keeps the last)
SELECT '{"a": 1, "a": 2}'::json;
-- scalars, arrays and nesting round-trip unchanged; numbers are not normalized
SELECT '[1, 2, ["x", null, true]]'::json;
SELECT '-0.50'::json AS num, '"hi\nthere"'::json AS str;
-- json -> text returns the stored text verbatim; json -> jsonb canonicalizes
SELECT '{"b":1,  "a":2}'::json::text AS as_text,
       '{"b":1,  "a":2}'::json::jsonb AS as_jsonb;
-- round-trip through a table column preserves the exact text
CREATE TABLE jtest (id int, doc json);
INSERT INTO jtest VALUES (1, '{"x": [1, 2, 3]}'), (2, 'null'), (3, '"hi"');
SELECT id, doc FROM jtest ORDER BY id;
DROP TABLE jtest;
-- input validation without throwing: well-formed vs malformed JSON
SELECT pg_input_is_valid('{"a": 1}', 'json') AS ok,
       pg_input_is_valid('{bad', 'json') AS bad,
       pg_input_is_valid('[1, 2', 'json') AS truncated;
-- the extraction operators return the VERBATIM source substring: the outer
-- whitespace is trimmed but the inner spelling survives untouched
SELECT '{"a":   [ 1,2 ]  , "b":2}'::json -> 'a' AS inner_ws;
-- a nested value is not re-rendered (jsonb would print it as '{"b": 1}')
SELECT '{"a":{"b":1}}'::json -> 'a' AS no_rerender;
-- json keeps duplicate keys, and the operator returns the LAST one
SELECT '{"a": 1,  "a": 2}'::json -> 'a' AS dup_last_wins;
-- numbers are not normalized, unlike the jsonb path
SELECT '{"a": 1.500}'::json ->> 'a' AS scale, '{"a": 1e2}'::json ->> 'a' AS exponent;
-- -> keeps a string quoted and escaped; ->> unquotes and unescapes it
SELECT '{"a": "x\ty"}'::json -> 'a' AS quoted,
       length('{"a": "x\ty"}'::json ->> 'a') AS unescaped_len;
-- a JSON null is the text 'null' through ->, but SQL NULL through ->>
SELECT '{"a": null}'::json -> 'a' AS raw, '{"a": null}'::json ->> 'a' AS as_text;
-- array subscripts are 0-based and count from the end when negative
SELECT '[ 1 ,  2 ]'::json -> 0 AS first, '[1,2,3]'::json -> -1 AS last,
       '[1,2,3]'::json -> 9 AS past_end;
-- a missing key or a wrong container kind is NULL, not an error
SELECT '{"a":1}'::json -> 'zz' AS missing, '1'::json -> 'a' AS scalar,
       '[1,2]'::json -> 'a' AS array_by_key, '{"a":1}'::json -> 0 AS object_by_index;
-- #> / #>> walk a text[] path; the empty path returns the whole value, trimmed
SELECT '{"a":{"b":["c","d"]}}'::json #> '{a,b,1}' AS path,
       '{"a":{"b":"q"}}'::json #>> '{a}' AS path_text;
SELECT '  {"a":1}  '::json #> '{}' AS empty_path;
-- a path element indexes an array only when it parses as an integer, and a
-- NULL element anywhere in the path makes the whole result NULL
SELECT '[1,2,3]'::json #> '{-1}' AS neg_step, '[1,2]'::json #> '{xx}' AS bad_step,
       '{"a":1}'::json #> ARRAY['a', NULL] AS null_step;
-- the operators chain left-to-right
SELECT '{"a":{"b":1}}'::json -> 'a' -> 'b' AS chained;
-- a \u0000 cannot become a text datum: json_in accepts the escape, but ->>
-- has to decode it. DIVERGENCE: PostgreSQL runs the whole document through a
-- validating lexer on every extraction, so it also rejects a \u0000 sitting in
-- some *other* key value, and rejects it on plain -> too. crabgresql decodes
-- only the string it actually returns, so it raises this for the matched value
-- alone and passes unrelated ones through untouched.
SELECT '{"a": "\u0000"}'::json ->> 'a';
-- extraction through a stored column (the value is decoded from the heap tuple)
CREATE TABLE jext (id int, doc json);
INSERT INTO jext VALUES (1, '{"x": [1,   2], "x": 9}'), (2, '{"y": "z"}');
SELECT id, doc -> 'x' AS x, doc #>> '{y}' AS y FROM jext ORDER BY id;
DROP TABLE jext;
-- json has no default btree operator class, so it cannot key a unique/primary
-- index (rejected at DDL rather than crashing at enforcement time)
CREATE TABLE jbad (j json UNIQUE);
CREATE TABLE jbad (j json PRIMARY KEY);
