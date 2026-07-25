--
-- TSQUERY
-- The text-search query type: input parsing, canonical output, the combinator
-- operators, and the `@@` match operator against a tsvector -- including phrase
-- search, prefix matching, weight filters and negation.
-- Output hand-checked against PostgreSQL.
--
-- A bare lexeme, and the quoting/escaping rules tsvector shares.
SELECT '1'::tsquery;
SELECT ' 1 '::tsquery;
SELECT '''1 2'''::tsquery;
SELECT $$'\\as'$$::tsquery;
-- `!` binds tightest, then `<->`, then `&`, then `|`. A child is parenthesized
-- only when it binds more loosely than its parent.
SELECT '!1'::tsquery;
SELECT '1|2'::tsquery;
SELECT '1&2'::tsquery;
SELECT '1|2&3'::tsquery;
SELECT '(1|2)&3'::tsquery;
SELECT '!(!1|!2)'::tsquery;
SELECT '!(1|2)&3'::tsquery;
SELECT '1&(2&(4&(5|!6)))'::tsquery;
-- Negation stacks without parentheses.
SELECT '!!b'::tsquery;
SELECT '!(!b)'::tsquery;
SELECT 'a & !!b'::tsquery;
SELECT '!(a&b)'::tsquery;
-- A phrase binds tighter than `&`/`|` and looser than `!`. Its *right* operand
-- is parenthesized when it is itself a phrase, so a left-nested chain prints
-- flat while a right-nested one keeps its parentheses.
SELECT '!a <-> b'::tsquery;
SELECT '(a<->b)&c'::tsquery;
SELECT 'a<->(b|c)'::tsquery;
SELECT 'a<->b<->c'::tsquery;
SELECT 'a<->(b<->c)'::tsquery;
SELECT 'a<2>b'::tsquery;
SELECT 'a<0>b'::tsquery;
-- Weight letters are a set: any order and spelling in, canonical ABCD out. The
-- prefix flag `*` may appear before or after them.
SELECT 'a:* & nbb:*ac | doo:a* | goo'::tsquery;
SELECT 'a:BA'::tsquery;
SELECT 'a:dcba'::tsquery;
SELECT 'a:*D'::tsquery;
-- errors: adjacent lexemes are not an implicit AND, and the phrase distance has
-- its own range check
SELECT 'a b'::tsquery;
SELECT 'foo!'::tsquery;
SELECT 'a &'::tsquery;
SELECT 'a <100000> b'::tsquery;
SELECT pg_input_is_valid('foo', 'tsquery') AS ok, pg_input_is_valid('foo!', 'tsquery') AS bad;
SELECT * FROM pg_input_error_info('foo!', 'tsquery');

-- numnode counts operators as well as lexemes; querytree shows the indexable
-- part, printing T when a negation leaves nothing to constrain a scan
SELECT numnode('new'::tsquery), numnode('new & york'::tsquery), numnode('new & york | qwery'::tsquery);
SELECT numnode('a <-> b'::tsquery) AS phrase, numnode('!!a'::tsquery) AS notnot;
SELECT querytree('foo & ! bar'::tsquery);
SELECT querytree('!foo'::tsquery);
SELECT querytree('a|!b'::tsquery);
SELECT querytree('a&(b|!c)'::tsquery);

-- combinators
SELECT 'foo & bar'::tsquery && 'asd';
SELECT 'foo & bar'::tsquery || 'asd & fg';
SELECT 'foo & bar'::tsquery || !!'asd & fg'::tsquery;
SELECT 'foo & bar'::tsquery && 'asd | fg';
SELECT 'a' <-> 'b & d'::tsquery;
SELECT 'a & g' <-> 'b <-> d'::tsquery;
SELECT tsquery_phrase('a <3> g', 'b & d', 10);
SELECT tsquery_phrase('a', 'b');

-- ordering: node count, then total lexeme bytes, then the operator
SELECT 'a' < 'b & c'::tsquery AS t, 'a' > 'b & c'::tsquery AS f;
SELECT 'a | f' < 'b & c'::tsquery AS t, 'a | ff' < 'b & c'::tsquery AS f;
SELECT 'a | f | g' < 'b & c'::tsquery AS f;
SELECT 'a <-> b'::tsquery < 'a & b'::tsquery AS t;

-- @@ : boolean matching, with weight filters and prefixes
SELECT 'a b:89  ca:23A,64b d:34c'::tsvector @@ 'd:AC & ca' AS "true";
SELECT 'a b:89  ca:23A,64b d:34c'::tsvector @@ 'd:AC & ca:C' AS "false";
SELECT 'a b:89  ca:23A,64b d:34c'::tsvector @@ 'd:AC & ca:CB' AS "true";
SELECT 'a b:89  ca:23A,64b d:34c'::tsvector @@ 'd:AC & c:*C' AS "false";
SELECT 'a b:89  ca:23A,64b d:34c'::tsvector @@ 'd:AC & c:*CB' AS "true";
SELECT 'supernova'::tsvector @@ 'super' AS "false";
SELECT 'supeznova supernova'::tsvector @@ 'super:*' AS "true";
SELECT 'wa:1A'::tsvector @@ '!w:*A' AS "false";
SELECT 'wa:1A'::tsvector @@ '!w:*D' AS "true";
-- the operand order may be written either way round
SELECT 'a & b'::tsquery @@ 'a:1 b:2'::tsvector AS "true";
-- a stripped vector has no weights, so weight filters are ignored on it
SELECT strip('wa:1A'::tsvector) @@ 'w:*A' AS "true";
SELECT strip('wa:1A'::tsvector) @@ 'w:*D' AS "true";
SELECT strip('wa:1A'::tsvector) @@ '!w:*D' AS "false";
-- An empty query matches nothing. PG also raises
-- `NOTICE: text-search query doesn't contain lexemes: ""` here; we do not yet,
-- because expression binding has no channel to emit a notice on.
SELECT 'a b c'::tsvector @@ ''::tsquery AS "false";

-- @@ : phrase search. `<N>` requires the operands exactly N positions apart.
SELECT 'a:1 b:2'::tsvector @@ 'a <-> b' AS "true";
SELECT 'a:1 b:2'::tsvector @@ 'a <0> b' AS "false";
SELECT 'a:1 b:2'::tsvector @@ 'a <2> b' AS "false";
SELECT 'a:1 b:3'::tsvector @@ 'a <2> b' AS "true";
SELECT 'a:1 b:3'::tsvector @@ 'a <0> a:*' AS "true";
SELECT 'wa:1D wb:2A'::tsvector @@ 'w:*D <-> w:*A' AS "true";
SELECT 'wa:1A wb:2D'::tsvector @@ 'w:*D <-> w:*A' AS "false";
-- a chained phrase measures from the whole matched span, so nesting to the
-- right still lines up with the outer operand
SELECT '1:1 2:2 3:3 4:4'::tsvector @@ '1 <-> 2 <-> 3' AS "true";
SELECT '1:1 2:2 3:3 4:4'::tsvector @@ '1 <-> (2 <-> 3)' AS "true";
SELECT '1:1 2:2 3:3 4:4'::tsvector @@ '1 <2> (2 <-> 3)' AS "false";
-- inside a phrase, `&` intersects and `|` unions the operands' positions
SELECT 'q:1 x:2 q:3 y:4'::tsvector @@ 'q <-> (x & y)' AS "false";
SELECT 'q:1 x:2'::tsvector @@ 'q <-> (x | y <-> z)' AS "true";
SELECT 'q:1 y:2 z:3'::tsvector @@ 'q <-> (x | y <-> z)' AS "true";
SELECT 'q:1 y:2 x:3'::tsvector @@ 'q <-> (x | y <-> z)' AS "false";
-- ... and `!` is the complement over the vector's positions
SELECT 'y:1 y:2 q:3'::tsvector @@ '(!x | y <-> z) <-> q' AS "true";
SELECT 'x:1 q:2'::tsvector @@ '(!x | y <-> z) <-> q' AS "false";
SELECT 'x:1 y:2 q:3 y:4'::tsvector @@ '!x <-> y' AS "true";
SELECT 'x:1 y:2 q:3 y:4'::tsvector @@ '!(x <-> y)' AS "false";
SELECT 'x:1 y:2 q:3 y:4'::tsvector @@ '!(x <2> y)' AS "true";
-- a query no lexeme contradicts matches even an empty vector
SELECT ''::tsvector @@ '!foo' AS "true";
-- without position data a phrase can never match
SELECT strip('x:1 y:2 q:3 y:4'::tsvector) @@ '!x <-> y' AS "false";
SELECT strip('x:1 y:2 q:3 y:4'::tsvector) @@ '!(x <-> y)' AS "true";

-- A query nested deeper than we can safely walk is refused rather than
-- overflowing the stack -- whether it nests by parentheses, by `!`, or by a
-- flat operator chain, which the parser builds with a loop but which every
-- later walk still recurses over.
--
-- Our cap is lower than PostgreSQL's: PG parses onto an explicit stack, so it
-- accepts these three and only reports `tsquery stack too small` far later (it
-- does reject the `!` case below). Bounding recursion is what keeps a single
-- literal from aborting the backend, so the cap stays.
SELECT (repeat('(', 5000) || 'a' || repeat(')', 5000))::tsquery;
SELECT (repeat('!', 5000) || 'a')::tsquery;
SELECT numnode((repeat('a&', 5000) || 'a')::tsquery);
-- a chain well inside the limit still works, and matches PG
SELECT numnode((repeat('a&', 400) || 'a')::tsquery);
-- tsquery_phrase enforces the same distance range as the `<N>` operator
SELECT tsquery_phrase('a', 'b', 20000);

-- a tsquery round-trips through text
SELECT 'a|b'::tsquery::text;
SELECT '''a'' | ''b'''::text::tsquery;

-- storage and filtering over a real table
CREATE TABLE tsq(id int, v tsvector, q tsquery);
INSERT INTO tsq VALUES (1, 'rat:1 cat:2A', 'rat & cat'), (2, 'dog:1', 'dog | fox'), (3, 'rat:1 cat:2A', 'rat <-> cat');
SELECT id, q FROM tsq ORDER BY id;
SELECT id FROM tsq WHERE v @@ q ORDER BY id;
SELECT id FROM tsq WHERE v @@ 'cat <-> rat'::tsquery ORDER BY id;
SELECT DISTINCT q FROM tsq ORDER BY q;
DROP TABLE tsq;

-- Storage keeps the tree shape. `'1|2|4'` and `'1|(2|4)'` print identically but
-- are distinct values, so a round trip through the heap must not collapse them.
CREATE TABLE tqshape(q tsquery);
INSERT INTO tqshape VALUES ('1|(2|4)'), ('1|2|4'), ('1&(2&4)'), ('1&2&4');
SELECT count(DISTINCT q) FROM tqshape;
SELECT q FROM tqshape ORDER BY q::text, q;
DROP TABLE tqshape;
