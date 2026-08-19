--
-- GREATEST / LEAST
-- The largest (smallest) argument, skipping NULLs: result-type resolution over
-- any arity, the type's own ordering, and the error cases.
--
-- the default column name is the keyword
SELECT GREATEST(1, 2, 3);
SELECT LEAST(1, 2, 3) AS smallest;
-- NULL arguments do not participate; all-NULL is NULL, and an all-untyped list
-- resolves to text as it does for COALESCE
SELECT GREATEST(1, NULL, 3) AS skips_null, LEAST(NULL, 2, NULL) AS also_skips;
SELECT LEAST(NULL, NULL) AS none, pg_typeof(LEAST(NULL, NULL)) AS ty;
-- one argument is legal
SELECT GREATEST(7) AS single;
-- arguments of differing numeric types resolve to the common type
SELECT GREATEST(1, 2.5) AS promoted, pg_typeof(GREATEST(1, 2.5)) AS ty;
SELECT pg_typeof(LEAST(1::int2, 2::bigint)) AS ty;
-- an untyped literal adapts to the resolved type per argument
SELECT GREATEST(NULL::int, '42') + 1 AS added;
-- every argument is evaluated, unlike COALESCE's
SELECT GREATEST(1, 1/0);
-- float8 puts NaN above every number, and bool has false < true
SELECT GREATEST('nan'::float8, 1) AS nan_wins, LEAST('nan'::float8, 1) AS number_wins;
SELECT GREATEST(false, true) AS t, LEAST('\x01'::bytea, '\x00') AS zero;
-- the answer moves with the derived collation: byte order (C) puts 'a' (0x61)
-- above 'B' (0x42), a linguistic one does not
SELECT GREATEST('B', 'a' COLLATE "C") AS c_order,
       GREATEST('B', 'a' COLLATE "en-US-x-icu") AS icu_order,
       LEAST('B', 'a' COLLATE "en-US-x-icu") AS icu_min;
-- "char" is a byte with its own unsigned ordering, and stays a byte -- unlike
-- min()/max(), which resolve through text
SELECT GREATEST('Z'::"char", '\377'::"char") AS high_byte,
       pg_typeof(GREATEST('Z'::"char", 'a'::"char")) AS ty;
-- an interval compares by its canonical span (30-day months, 24-hour days),
-- so '2 hours' is the smaller of the two
SELECT GREATEST('2024-03-01'::date, '2024-01-01'::date) AS later,
       LEAST('1 day'::interval, '2 hours'::interval) AS shorter;
-- over a table column, once per row
CREATE TABLE readings (station text, low integer, high integer, tag varchar(3),
                       other_tag varchar(3), long_tag varchar(5));
INSERT INTO readings VALUES ('a', 3, 12, 'ab', 'zz', 'ab'),
                            ('b', NULL, 7, 'cd', 'aa', 'cd'),
                            ('c', -3, NULL, 'ef', 'mm', 'ef');
SELECT station, GREATEST(low, high) AS hi, LEAST(low, high) AS lo
  FROM readings ORDER BY station;
SELECT station FROM readings WHERE GREATEST(low, high) > 7 ORDER BY station;
SELECT sum(LEAST(low, high, 0)) AS total FROM readings;
SELECT GREATEST(low, high) AS hi, count(*) AS n
  FROM readings GROUP BY GREATEST(low, high) ORDER BY 1;
-- error: incompatible concrete argument types cannot be matched, reported under
-- the keyword. Known gap: PG also prints a `LINE 1: ... ^` cursor under the
-- offending argument, which the result-type unifier does not carry a span for
SELECT GREATEST(1, 'abc'::text);
SELECT LEAST(1, 'abc'::text);
-- error: an untyped argument that does not fit the resolved type
SELECT GREATEST(1, 'x');
-- error: the resolved type has no ordering at all (json), or has equality
-- without one (xid). Divergence: PG raises this when it initializes the
-- expression, so `CREATE VIEW v AS SELECT greatest('{}'::json, '{}')` is
-- accepted there and fails on `SELECT * FROM v`; here it is refused at bind time
SELECT GREATEST('{}'::json, '{}'::json);
SELECT LEAST('1'::xid, '2'::xid);
-- error: at least one argument is required. PG rejects this in the grammar and
-- prints the same message, with a cursor under the paren
SELECT GREATEST();
-- error: two explicit collations that disagree (PG prints a cursor here too)
SELECT LEAST('a' COLLATE "C", 'b' COLLATE "POSIX");
-- both are keywords, so they are spelled in any case ...
SELECT GrEaTeSt(1, 2) AS keyword_case;
-- ... but only bare: a quoted or schema-qualified name is an ordinary function
-- lookup, and no schema holds a function by that name. Known gap: for the
-- qualified form PG names the missing function `pg_catalog.greatest(integer,
-- integer)`, and both add a cursor and a HINT
SELECT pg_catalog.greatest(1, 2);
SELECT "least"(1, 2);
-- every decoration a function call may carry is a syntax error in PG's grammar.
-- Known gap: PG echoes the token as written, ours always in lower case
SELECT GREATEST(1, 2) OVER ();
SELECT GREATEST(DISTINCT 1, 2);
SELECT LEAST(1, 2) FILTER (WHERE true);
-- deparse: PG prints the keyword in upper case and labels every literal
-- argument with the type the list resolved to
CREATE TABLE defaults (x integer DEFAULT GREATEST(1, NULL),
                       y text DEFAULT LEAST('b', 'a'));
SELECT pg_get_expr(adbin, adrelid) AS deparsed
  FROM pg_attrdef d JOIN pg_class c ON c.oid = d.adrelid
 WHERE c.relname = 'defaults' ORDER BY adnum;
DROP TABLE defaults;
-- a view keeps the spelling, and a type modifier survives only when every
-- argument agrees on it. Two known gaps COALESCE shares: this path does not
-- type-label the literals (`NULL`, not `NULL::integer`), and a *folded* constant
-- loses the cast that carried its modifier -- hence the column references here
CREATE VIEW v AS
  SELECT GREATEST(1, low, NULL) AS g, LEAST('b'::text, 'a') AS l,
         GREATEST(tag, other_tag) AS same_typmod,
         GREATEST(tag, long_tag) AS mixed_typmod
    FROM readings;
SELECT pg_get_viewdef('v', true);
SELECT attname, format_type(atttypid, atttypmod) AS spelling
  FROM pg_attribute WHERE attrelid = 'v'::regclass AND attnum > 0 ORDER BY attnum;
DROP VIEW v;
DROP TABLE readings;
