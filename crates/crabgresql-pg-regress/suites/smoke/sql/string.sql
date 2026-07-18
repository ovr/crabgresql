--
-- STRING
-- Character types (text / varchar / char / bpchar / name) and the string
-- functions: concatenation, length, case, substring/position/overlay, the
-- trim/pad family, replace/translate/repeat/reverse/left/right, ascii/chr,
-- split_part/starts_with/to_hex, concat/concat_ws/format, LIKE/ILIKE,
-- encode/decode, and the quoting functions. Output hand-checked against
-- PostgreSQL.
--

-- concatenation: text || text, and text || non-text (either side)
SELECT 'foo' || 'bar' AS cat, 'n=' || 42 AS with_int, 1.5 || 'x' AS num_left;
SELECT NULL || 'a' AS null_concat, 'a' || NULL AS concat_null;

-- length family: characters vs bytes vs bits
SELECT length('café') AS len, char_length('café') AS clen,
       octet_length('café') AS olen, bit_length('café') AS blen;

-- case folding and initcap
SELECT upper('Hello, World') AS up, lower('Hello, World') AS lo,
       initcap('hi THERE o''brien') AS ic;

-- substring: functional and SQL syntax forms; substr indexing is 1-based
SELECT substr('abcdef', 2, 3) AS s1, substr('abcdef', 2) AS s2,
       substring('abcdef' FROM 2 FOR 3) AS s3, substring('abcdef' FROM 3) AS s4;
SELECT substr('abcdef', 0, 2) AS clamp_start, substr('abcdef', -1) AS neg_start;

-- position / strpos (note the reversed argument order)
SELECT position('cd' IN 'abcdef') AS pos, strpos('abcdef', 'cd') AS sp,
       position('z' IN 'abcdef') AS absent, strpos('abc', '') AS empty;

-- overlay
SELECT overlay('Txxxxas' PLACING 'hom' FROM 2 FOR 4) AS ov1,
       overlay('abcdef' PLACING 'XY' FROM 3) AS ov2;

-- trim family and padding
SELECT trim(both 'x' FROM 'xxabcxx') AS btrim_x, trim(leading FROM '  ab') AS ltrim_sp,
       trim(trailing FROM 'ab  ') || '|' AS rtrim_sp;
SELECT ltrim('yyxabc', 'xy') AS lt, rtrim('abcyx', 'xy') AS rt, btrim('  ab  ') AS bt;
SELECT lpad('abc', 6, '*') AS lp, rpad('abc', 6, '*') AS rp,
       lpad('abcdef', 3) AS lp_trunc, lpad('ab', 5, '') AS lp_emptyfill;

-- replace / translate / repeat / reverse / left / right
SELECT replace('abcabc', 'bc', 'XY') AS rep, translate('12345', '143', 'ax') AS tr,
       repeat('ab', 3) AS rpt, reverse('abcdef') AS rev;
SELECT left('abcdef', 2) AS l1, left('abcdef', -2) AS l2,
       right('abcdef', 2) AS r1, right('abcdef', -2) AS r2;

-- ascii / chr / split_part / starts_with / to_hex
SELECT ascii('A') AS a1, ascii('') AS a2, chr(65) AS c1, chr(233) AS c2;
SELECT split_part('a,b,c,d', ',', 2) AS sp1, split_part('a,b,c,d', ',', -1) AS sp_last,
       split_part('a,b', ',', 5) AS sp_oob;
SELECT starts_with('abcdef', 'abc') AS sw1, starts_with('abcdef', 'xyz') AS sw2,
       to_hex(255) AS h1, to_hex(-1) AS h2;

-- concat / concat_ws / format (NULL-tolerant)
SELECT concat('a', NULL, 2, 'b') AS c, concat_ws('-', 'x', NULL, 'y', 'z') AS cw;
SELECT format('%s has %s items (%I=%L)', 'cart', 3, 'q', 'a''b') AS f1,
       format('%1$s-%1$s', 'z') AS f2;

-- LIKE / ILIKE, with % / _ wildcards, ESCAPE, and NOT
SELECT 'abc' LIKE 'a%' AS l1, 'abc' LIKE 'a_c' AS l2, 'abc' LIKE 'a_' AS l3,
       'ABC' ILIKE 'a%c' AS l4, 'abc' NOT LIKE 'x%' AS l5;
SELECT 'a%b' LIKE 'a\%b' AS esc1, 'axb' LIKE 'a\%b' AS esc2,
       'a%b' LIKE 'a$%b' ESCAPE '$' AS esc3;

-- encode / decode
SELECT encode('\x001000'::bytea, 'hex') AS e_hex, encode('abc'::bytea, 'base64') AS e_b64,
       encode('a\000b'::bytea, 'escape') AS e_esc;
SELECT decode('001000', 'hex') AS d_hex, decode('YWJj', 'base64') AS d_b64;

-- quoting
SELECT quote_ident('foo') AS qi1, quote_ident('foo bar') AS qi2,
       quote_literal('a''b') AS ql, quote_nullable(NULL) AS qn;

-- character types: casts, typmod truncation, and the default column names
SELECT 'abc'::varchar AS v_unbounded, 'abcdef'::varchar(3) AS v_trunc,
       'abcdef'::char(3) AS c_trunc, 'ab'::char AS c_default;
SELECT '[' || 'ab'::char(5) || ']' AS char_padded,
       octet_length('ab'::char(5)) AS char_octets, length('ab'::char(5)) AS char_len;
SELECT 'a name value'::name AS nm, length(repeat('x', 70)::name) AS name_trunc;

-- bpchar comparison ignores trailing blanks; varchar/text do not
SELECT 'ab'::char(4) = 'ab'::char(2) AS bpchar_eq, 'ab '::varchar = 'ab' AS varchar_ne;

-- character columns: char(n) pads on INSERT, varchar(n) length is enforced
CREATE TEMP TABLE str_t (c char(5), v varchar(3), n name);
INSERT INTO str_t VALUES ('ab', 'xy', 'nm');
SELECT '[' || c || ']' AS c, '[' || v || ']' AS v, octet_length(c) AS c_octets FROM str_t;
INSERT INTO str_t (c) VALUES ('toolong');
INSERT INTO str_t (v) VALUES ('abcdef');

-- format() field widths (right/left-justified, and a `*` width argument)
SELECT format('[%5s]', 'x') AS w1, format('[%-5s]', 'x') AS w2, format('[%*s]', 4, 'y') AS w3;

-- errors: chr(0)/surrogate, negative substring length, split_part field 0,
-- overlay start below 1, oversized result, and malformed encode/decode input
SELECT chr(0);
SELECT chr(55296);
SELECT substr('abc', 1, -1);
SELECT split_part('a,b', ',', 0);
SELECT overlay('abc' PLACING 'X' FROM 0);
SELECT repeat('a', 2147483647);
SELECT decode('a@b', 'base64');
SELECT decode('xyz', 'hex');
