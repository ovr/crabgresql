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
