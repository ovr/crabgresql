--
-- JSONPATH
-- The SQL/JSON path language and the jsonb_path_* functions / @? @@ operators.
-- Covers jsonpath I/O (canonical output), casts, navigation, filters, item
-- methods, arithmetic, lax vs strict, variables, silent mode, and the operator
-- forms. Output hand-checked against PostgreSQL.
--
-- jsonpath input is re-emitted in canonical form: keys are always double-quoted,
-- arithmetic is fully parenthesized, and 'lax' is the (elided) default.
SELECT '$.a.b[*] ? (@ > 3)'::jsonpath;
SELECT 'lax $."a b"[1 to 3, 5].size()'::jsonpath;
SELECT 'strict $.a.**{2 to 4}.c'::jsonpath;
SELECT '$.a + $.b * 2 - (-3)'::jsonpath AS arith;
SELECT '$ ? (@ like_regex "ab.*c" flag "i")'::jsonpath AS re;
-- like_regex flags are re-emitted from the parsed set: fixed order, deduped,
-- and omitted when empty
SELECT '$ ? (@ like_regex "a" flag "qmi")'::jsonpath AS re_ord,
       '$ ? (@ like_regex "a" flag "ii")'::jsonpath AS re_dup,
       '$ ? (@ like_regex "a" flag "")'::jsonpath AS re_none,
       '$ ? (@ like_regex "a" flag "xq")'::jsonpath AS re_xq;
-- like_regex flags are validated when the path is parsed, not when it is used
SELECT '$ like_regex "a" flag "z"'::jsonpath;
SELECT '$ like_regex "a" flag "x"'::jsonpath;
-- ... so an empty array, which iterates nothing, still raises
SELECT jsonb_path_match('[]'::jsonb, '$[*] ? (@ like_regex "a b" flag "x")'::jsonpath);
SELECT pg_input_is_valid('$ like_regex "a" flag "z"', 'jsonpath') AS bad_flag,
       pg_input_is_valid('$ like_regex "a" flag "qx"', 'jsonpath') AS ok_flag;
-- q matches the pattern literally, and composes with i
SELECT jsonb_path_match('"A.C"'::jsonb, '$ like_regex "a.c" flag "qi"') AS q_i,
       jsonb_path_match('"ABC"'::jsonb, '$ like_regex "a.c" flag "q"') AS q_only,
       jsonb_path_match('"a\nb"'::jsonb, '$ like_regex "^b" flag "m"') AS m_flag,
       jsonb_path_match('"a\nb"'::jsonb, '$ like_regex "a.b"') AS no_s;
SELECT '$ ? (@.name starts with "Jo")'::jsonpath AS sw;
SELECT '$ ? (exists (@.x))'::jsonpath AS ex;
SELECT '$[last]'::jsonpath AS last;
-- a jsonpath round-trips through text
SELECT '$.a[*]'::jsonpath::text AS as_text;
-- pg_input_is_valid reports a bad path without raising
SELECT pg_input_is_valid('$.a', 'jsonpath') AS ok, pg_input_is_valid('$.', 'jsonpath') AS bad;

-- jsonb_path_query: navigation and filters return an SQL/JSON sequence
SELECT jsonb_path_query('{"a":[1,2,3]}', '$.a[*] ? (@ > 1)');
SELECT jsonb_path_query('[{"x":1},{"x":9}]', '$ ? (@.x > 5)');
-- recursive .** descends into every level
SELECT jsonb_path_query('{"a":{"b":1,"c":{"b":2}}}', '$.**.b');
-- item methods
SELECT jsonb_path_query('[1,"x",true,null,{},[2]]', '$[*].type()');
SELECT jsonb_path_query('[1,2,3]', '$.size()') AS size,
       jsonb_path_query('"1.5"', '$.double()') AS dbl,
       jsonb_path_query('-2.3', '$.abs()') AS abs;
SELECT jsonb_path_query('{"a":1,"b":2}', '$.keyvalue()');
-- arithmetic (numeric scale is preserved, matching jsonb numbers)
SELECT jsonb_path_query('{"a":5,"b":2}', '$.a + $.b') AS sum,
       jsonb_path_query('{}', '(1.50 + 2.5)') AS scaled,
       jsonb_path_query('{}', '(7 % 3)') AS modulo;
-- a predicate path yields a single boolean item (unknown -> json null)
SELECT jsonb_path_query('{"a":1}', '$.a > 0') AS t,
       jsonb_path_query('{"a":"x"}', '$.a > 1') AS unknown;

-- jsonb_path_query_array wraps the matches; _first returns the first (or NULL)
SELECT jsonb_path_query_array('{"a":[1,2]}', '$.a[*]') AS arr,
       jsonb_path_query_first('{"a":[5,6]}', '$.a[*]') AS first,
       jsonb_path_query_first('{"a":[]}', '$.a[*]') AS none;

-- jsonb_path_exists / jsonb_path_match and their operator forms @? / @@
SELECT jsonb_path_exists('{"a":1}', '$.a') AS y,
       jsonb_path_exists('{"a":1}', '$.b') AS n;
SELECT jsonb_path_match('{"a":1}', '$.a == 1') AS t,
       jsonb_path_match('{"a":1}', '$.a == 2') AS f;
-- a comparison whose operand types differ is unknown -> SQL NULL
SELECT jsonb_path_match('{"a":"x"}', '$.a > 1') IS NULL AS is_null;
SELECT '{"a":1}'::jsonb @? '$ ? (@.a > 0)' AS e,
       '{"a":"hello"}'::jsonb @@ '$.a starts with "he"' AS m;
-- the @? / @@ operators are silent: a strict structural error becomes NULL
SELECT '1'::jsonb @? 'strict $.a' AS q, '1'::jsonb @@ 'strict $.a == 1' AS a;

-- variables are supplied via the vars argument
SELECT jsonb_path_query('{"x":5}', '$.x ? (@ >= $min)', '{"min":3}') AS withvars;
-- lax (default) suppresses structural errors; strict raises them
SELECT jsonb_path_query('1', '$.a') AS lax_empty;
SELECT jsonb_path_query('{"a":1}', 'strict $.b');
-- silent => true suppresses the same error
SELECT jsonb_path_exists('{"a":1}', 'strict $.b', '{}', true) AS silent;
-- out-of-bounds subscript: strict errors, lax skips (yielding no rows)
SELECT jsonb_path_query('[1,2]', 'strict $[5]');
SELECT * FROM jsonb_path_query('[1,2]', 'lax $[5]') AS t(v);

-- jsonb_path_query in FROM position, with a column alias
SELECT * FROM jsonb_path_query('{"a":[10,20,30]}', '$.a[*] ? (@ >= 20)') AS t(v);

-- the jsonb_path_* functions are STRICT: a NULL vars/silent argument makes the
-- whole result NULL (and the set-returning form yields no rows)
SELECT jsonb_path_exists('{"a":1}', '$.a', NULL) AS ex_null,
       jsonb_path_match('{"a":1}', '$.a == 1', '{}', NULL) AS m_null;
SELECT * FROM jsonb_path_query('1', '$', NULL) AS t(v);

-- lax mode unwraps a bare array (one level) for the numeric / keyvalue methods
SELECT jsonb_path_query('[1,-2,3]', '$.abs()') AS abs,
       jsonb_path_query('[1.5,2.5]', '$.floor()') AS floor;
-- but only one level: an array element that is itself an array errors
SELECT jsonb_path_query('[[1],[-2]]', '$.abs()');
-- .size()/.type() describe the container and never unwrap
SELECT jsonb_path_query('[1,2,3]', '$.size()') AS size,
       jsonb_path_query('[1,2]', '$.type()') AS type;
-- strict .size() on a non-array is an error
SELECT jsonb_path_query('5', 'strict $.size()');

-- an arithmetic "single numeric value" error names the actual operator, and
-- unary minus has its own wording
SELECT jsonb_path_query('[1,2]', '$[*] * 1');
SELECT jsonb_path_query('{"a":"x"}', '- $.a');

-- errors: a member method on the wrong kind, and a bad path literal
SELECT jsonb_path_query('"x"', '$.abs()');
SELECT '$ ? (@ >)'::jsonpath;
