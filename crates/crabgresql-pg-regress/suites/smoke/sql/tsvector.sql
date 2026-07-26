--
-- TSVECTOR
-- The text-search document type: input parsing, canonical output, the editing
-- functions, ordering and storage. The query side lives in tsquery.
-- Output hand-checked against PostgreSQL.
--
-- Input is whitespace-separated; every lexeme comes back single-quoted, sorted
-- by byte order, with duplicates merged.
SELECT '1'::tsvector;
SELECT ' 1 '::tsvector;
SELECT '1 2'::tsvector;
SELECT 'b a'::tsvector;
SELECT 'a b a'::tsvector;
-- An empty input is a valid empty tsvector.
SELECT ''::tsvector;
-- A lexeme may be single-quoted, which lets it contain spaces; `''` is an
-- embedded quote and a quoted lexeme is self-delimiting.
SELECT '''1 2'''::tsvector;
SELECT '''1 ''''2'''::tsvector;
SELECT '''1 ''''2''3'::tsvector;
-- Backslash escapes one character in either form; output re-doubles them.
SELECT $$'\\as' ab\c ab\\c AB\\\c ab\\\\c$$::tsvector;
-- A lexeme may carry positions, which sort and de-duplicate.
SELECT 'a:2,1'::tsvector;
SELECT 'a:1,1'::tsvector;
SELECT 'a:1 a:2'::tsvector;
-- A positionless duplicate does not erase the positions.
SELECT 'a:1 a'::tsvector;
-- Weights rank D < C < B < A; D is the default and is never printed. A repeated
-- position keeps the strongest weight.
SELECT '''w'':4A,3B,2C,1D,5 a:8'::tsvector;
SELECT 'a:1B a:1A'::tsvector;
SELECT 'a:1D a:1C'::tsvector;
-- `*` is an accepted spelling of weight A. A `D` keeps the default and lets
-- another weight letter follow, so `1dc` is C -- but `1cd` is a syntax error.
SELECT 'a:1*'::tsvector;
SELECT 'a:1dc'::tsvector;
SELECT 'a:1cd'::tsvector;
-- Positions cap at 16383 silently, but 0 is rejected.
SELECT 'a:16384'::tsvector;
SELECT 'a:0'::tsvector;
-- An empty lexeme can never be stored.
SELECT $$''$$::tsvector;
SELECT 'a:'::tsvector;
SELECT 'a:b'::tsvector;
-- pg_input_is_valid reports bad input without raising.
SELECT pg_input_is_valid('foo', 'tsvector') AS ok, pg_input_is_valid($$''$$, 'tsvector') AS bad;
SELECT * FROM pg_input_error_info($$''$$, 'tsvector');

-- concatenation shifts the right operand past the left's highest position, and
-- normalizes a lowercase weight letter
SELECT 'a:3A b:2a'::tsvector || 'ba:1234 a:1B';
SELECT 'a b'::tsvector || 'b:1 c'::tsvector;
SELECT 'a:1 b:2'::tsvector || ''::tsvector;

-- editing functions
SELECT strip('w:12B w:13* w:12,5,6 a:1,3* a:3 w asd:1dc asd'::tsvector);
SELECT length('a b c'::tsvector) AS three, length(''::tsvector) AS zero;
SELECT setweight('a:1,3A asd:1C w:5,6,12B,13A'::tsvector, 'c');
-- the three-argument form stamps only the named lexemes; NULL entries are skipped
SELECT setweight('a:1,3A asd:1C w:5,6,12B,13A zxc:81,222A,567'::tsvector, 'c', '{a,zxc}');
SELECT setweight('a asd w:5,6,12B,13A zxc'::tsvector, 'c', ARRAY['a', 'zxc', '', NULL]);
SELECT setweight('a:1'::tsvector, 'x');
-- ts_delete matches whole lexemes only: neither a prefix nor a plural removes one
SELECT ts_delete('base:7 hidden:6 rebel:1 spaceship:2,33A,34B,35C,36D strike:3'::tsvector, 'bas');
SELECT ts_delete('base:7 hidden:6 rebel:1 spaceship:2,33A,34B,35C,36D strike:3'::tsvector, 'spaceship');
SELECT ts_delete('base hidden rebel spaceship strike'::tsvector, ARRAY['spaceship','leya','rebel', '', NULL]);
-- ts_filter keeps the listed weights; a stripped lexeme has none, so it drops
SELECT ts_filter('base:7A empir:17 evil:15 hidden:6A rebel:1A won:9'::tsvector, '{a}');
SELECT ts_filter('base hidden rebel'::tsvector, '{a}');
SELECT ts_filter('base:7A'::tsvector, '{a,x}');
-- Unlike setweight/ts_delete, ts_filter rejects a NULL weight. PostgreSQL
-- spells the argument `"char"[]`, a type this engine does not have, so we model
-- it as text[] -- which is why PG answers this particular call with
-- `function ts_filter(tsvector, text[]) does not exist` instead.
SELECT ts_filter('base:7A'::tsvector, ARRAY['a', NULL]);
-- array conversions; array_to_tsvector sorts and de-duplicates
SELECT tsvector_to_array('base:7 hidden:6 rebel:1'::tsvector);
SELECT array_to_tsvector(ARRAY['foo','bar','baz','bar']);
SELECT array_to_tsvector(ARRAY['base', NULL]);
SELECT array_to_tsvector(ARRAY['base', '']);

-- a tsvector round-trips through text
SELECT 'a:1B b'::tsvector::text;
SELECT 'a:1B b'::text::tsvector;

-- ordering: storage footprint first, then lexeme count, then byte order. The
-- footprint outranks the count, so one long lexeme sorts after two short ones.
SELECT v FROM (VALUES ('b'::tsvector),('aa'),('a b'),('zz'),('a:1')) t(v) ORDER BY v;
SELECT 'aaaaaaa'::tsvector < 'a b'::tsvector AS f, 'a:1'::tsvector < 'a b'::tsvector AS t;
SELECT 'aa:1'::tsvector < 'b:1'::tsvector AS t, 'a'::tsvector < 'a b'::tsvector AS t2;
-- positions order descending, so 'a':2 precedes 'a':1
SELECT v FROM (VALUES ('a:1'::tsvector),('a:2'),('a:3,4'),('a:1,2')) t(v) ORDER BY v;
-- positions participate in equality
SELECT 'a:1'::tsvector = 'a:1'::tsvector AS t, 'a:1'::tsvector = 'a:2'::tsvector AS f;
-- a text operand is not silently reinterpreted as a tsvector: PG's `||` on
-- text and tsvector is the text concatenation, not a lexeme union
SELECT 'hello'::text || 'a b'::tsvector;

-- storage, grouping and indexing over a real table
CREATE TABLE tsdoc(id int, v tsvector);
INSERT INTO tsdoc VALUES (1, 'rat:1 cat:2A'), (2, 'dog:1'), (3, 'rat:1 cat:2A');
SELECT id, v FROM tsdoc ORDER BY id;
SELECT v, count(*) FROM tsdoc GROUP BY v ORDER BY v;
SELECT DISTINCT v FROM tsdoc ORDER BY v;
CREATE INDEX tsdoc_v_idx ON tsdoc(v);
SELECT id FROM tsdoc WHERE v = 'cat:2A rat:1'::tsvector ORDER BY id;
DROP TABLE tsdoc;
