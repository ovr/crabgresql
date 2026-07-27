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

-- POSIX regex operators: ~ / ~* / !~, with anchors and case-insensitivity
SELECT 'abc' ~ 'b' AS r1, 'abc' ~ '^a' AS r2, 'abc' ~ '^b' AS r3,
       'ABC' ~* 'abc' AS r4, 'abc' !~ 'z' AS r5;
-- SIMILAR TO: whole-string match, alternation, _ wildcard, NOT, ESCAPE
SELECT 'abc' SIMILAR TO '(b|a)%' AS s1, 'abc' SIMILAR TO 'a_c' AS s2,
       'abc' SIMILAR TO 'a' AS s3, 'abc' NOT SIMILAR TO 'x%' AS s4,
       'a%c' SIMILAR TO 'a$%c' ESCAPE '$' AS s5;
-- SIMILAR TO bracket expressions, quantifier bounds, and escaped metacharacters
SELECT 'b' SIMILAR TO '[^a]' AS b1, '%' SIMILAR TO '[%_]' AS b2,
       'aa' SIMILAR TO 'a{2}' AS b3, 'a{c' SIMILAR TO 'a{c' AS b4,
       'a|b' SIMILAR TO 'a\|b' AS b5;

-- regexp_replace: first match only, then every match with the g flag
SELECT regexp_replace('a1b2', '[0-9]', 'X') AS rr1,
       regexp_replace('a1b2', '[0-9]', 'X', 'g') AS rr2,
       regexp_replace('abc', 'B', 'X', 'i') AS rr3;
-- the PG replacement escapes: \1..\9 groups, \& whole match, \\ backslash
SELECT regexp_replace('1112223333', '(\d{3})(\d{3})(\d{4})', '(\1) \2-\3') AS rr4,
       regexp_replace('abc', 'b', '[\&]') AS rr5,
       regexp_replace('abc', '(b)', '[\10]') AS rr6,
       regexp_replace('abc', 'b', '[\q]') AS rr7;
-- regexp_like / regexp_count / regexp_substr, incl. start, n and subexpr
SELECT regexp_like('abc', 'B', 'i') AS rl1, regexp_like('abc', 'B') AS rl2,
       regexp_count('abcabc', 'a') AS rc1, regexp_count('abcabc', 'a', 2) AS rc2,
       regexp_count('abcABC', 'a', 1, 'i') AS rc3;
SELECT regexp_substr('abcdef', 'c.') AS rs1,
       regexp_substr('foobarbaz', 'b(a)(.)', 1, 2, 'i', 2) AS rs2,
       regexp_substr('abc', 'z') AS rs3, regexp_substr('abc', '(x)?b', 1, 1, '', 1) AS rs4;
-- start re-seeds the scan, so a match that began earlier is clipped not skipped
SELECT regexp_substr('hello world', '[a-z]+', 3) AS st1,
       regexp_substr('aaaaa', 'aa', 2, 2) AS st2,
       regexp_count('aaaaa', 'aa', 2) AS st3,
       regexp_count('abcabc', '^a', 2) AS st4;
-- a pattern with no capture groups treats subexpr 1 as the whole match
SELECT regexp_substr('abc', 'b', 1, 1, '', 1) AS sx1,
       regexp_substr('abc', 'b', 1, 1, '', 2) AS sx2;
-- the x flag keeps whitespace significant inside a bracket expression
SELECT regexp_like('a b', 'a[ ]b', 'x') AS x1, regexp_like('ab', 'a b', 'x') AS x2;
-- newline-sensitive modes stop a negated class from matching a newline
SELECT regexp_like(chr(10), '[^x]') AS n1, regexp_like(chr(10), '[^x]', 'n') AS n2,
       regexp_replace('a'||chr(10)||'b', '[^x]b', 'X', 'n') = 'a'||chr(10)||'b' AS n3;
-- errors: unknown flag, the global flag where it makes no sense, bad start
SELECT regexp_replace('abc', 'b', 'x', 'z');
SELECT regexp_like('abc', 'b', 'g');
SELECT regexp_count('abc', 'b', 0);
-- start is validated before the flags string
SELECT regexp_count('abc', 'b', 0, 'z');

-- encode / decode
SELECT encode('\x001000'::bytea, 'hex') AS e_hex, encode('abc'::bytea, 'base64') AS e_b64,
       encode('a\000b'::bytea, 'escape') AS e_esc;
SELECT decode('001000', 'hex') AS d_hex, decode('YWJj', 'base64') AS d_b64;

-- quoting
SELECT quote_ident('foo') AS qi1, quote_ident('foo bar') AS qi2,
       quote_literal('a''b') AS ql, quote_nullable(NULL) AS qn;

-- bool -> text spells the value out, unlike the t/f output function that still
-- backs display, concat() and the cast to name
SELECT true::text AS b1, false::varchar AS b2, true::name AS b3,
       concat(true, 'x') AS b4, true || 'x' AS b5, length(true::text) AS b6;

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
