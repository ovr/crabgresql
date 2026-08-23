--
-- ARRAY_AGG
-- The one aggregate that does not skip a NULL input: a NULL row becomes a NULL
-- element, while a group with no rows at all is still NULL rather than the empty
-- array. Covers the element types, GROUP BY / HAVING / ORDER BY, the DISTINCT
-- form (which PostgreSQL sorts, NULLs last), the window form, the calls that are
-- errors in PostgreSQL too, and the shapes only this build refuses — each of
-- those noted with the answer PostgreSQL gives.
-- Generated with psql -q -a against PostgreSQL 18.4.
--
CREATE TABLE aa (id integer, grp integer, val integer, txt text);
INSERT INTO aa VALUES
  (1, 1, 10, 'apple'),
  (2, 1, NULL, 'banana'),
  (3, 2, 5, NULL),
  (4, 2, 5, 'cherry'),
  (5, 3, 7, 'date');
-- NULL inputs are kept, and the elements follow the scan order
SELECT array_agg(val) FROM aa;
SELECT array_agg(txt) FROM aa;
-- an empty group is NULL, not {}
SELECT array_agg(val) FROM aa WHERE false;
-- a group of one NULL row is {NULL}, which is what the empty group differs from
SELECT array_agg(val) FROM aa WHERE id = 2;
-- the result type is the array over the argument type
SELECT pg_typeof(array_agg(val)), pg_typeof(array_agg(txt)) FROM aa;
SELECT pg_typeof(array_agg(val::smallint)), pg_typeof(array_agg(val::bigint)) FROM aa;
-- element types across the board; numeric keeps its display scale
SELECT array_agg(v) FROM (VALUES (1::numeric), (2.50), (NULL)) t(v);
SELECT array_agg(v) FROM (VALUES (1.5::float8), ('NaN'::float8)) t(v);
SELECT array_agg(v) FROM (VALUES (true), (false), (NULL)) t(v);
SELECT array_agg(v) FROM (VALUES ('2001-02-03'::date), (NULL)) t(v);
SELECT array_agg(v) FROM (VALUES ('2001-02-03 04:05:06'::timestamp)) t(v);
-- array_out quoting: a comma, an empty string and the literal word NULL are quoted
SELECT array_agg(v) FROM (VALUES ('a'), ('b, c'), (''), ('NULL'), (NULL)) t(v);
-- an expression argument, not just a column
SELECT array_agg(val * 2) FROM aa;
-- GROUP BY, one array per group
SELECT grp, array_agg(val) FROM aa GROUP BY grp ORDER BY grp;
SELECT grp, array_agg(txt) FROM aa GROUP BY grp ORDER BY grp;
-- HAVING over the aggregate's own argument
SELECT grp, array_agg(val) FROM aa GROUP BY grp HAVING count(val) > 1 ORDER BY grp;
-- ORDER BY the aggregate: arrays compare element-wise, shorter is less on a tie
SELECT grp, array_agg(val) FROM aa GROUP BY grp ORDER BY 2;
-- the aggregate's own ORDER BY decides the element order, NULLs placed by the key
SELECT array_agg(val ORDER BY val) FROM aa;
SELECT array_agg(val ORDER BY val DESC) FROM aa;
SELECT array_agg(val ORDER BY val NULLS FIRST) FROM aa;
-- ordered by a column the result never shows, and by more than one key
SELECT array_agg(txt ORDER BY val, id DESC) FROM aa;
-- per group, and next to another aggregate over the same group
SELECT grp, array_agg(val ORDER BY val DESC) FROM aa GROUP BY grp ORDER BY grp;
SELECT count(val), sum(val), array_agg(val ORDER BY val) FROM aa;
-- DISTINCT with an ORDER BY over the argument: PostgreSQL sorts by the key
SELECT array_agg(DISTINCT val ORDER BY val DESC) FROM aa;
-- ... but the expressions must be the arguments, since the dedup *is* that sort
SELECT array_agg(DISTINCT val ORDER BY id) FROM aa;
-- DISTINCT: PostgreSQL sorts the result and puts NULLs last
SELECT array_agg(DISTINCT val) FROM aa;
SELECT array_agg(DISTINCT txt) FROM aa;
SELECT grp, array_agg(DISTINCT val) FROM aa GROUP BY grp ORDER BY grp;
-- alongside the other aggregates over the same group
SELECT grp, count(*), sum(val), array_agg(val) FROM aa GROUP BY grp ORDER BY grp;
-- the window form: the default frame makes it a running array
SELECT id, array_agg(val) OVER (ORDER BY id) FROM aa ORDER BY id;
SELECT id, array_agg(txt) OVER (PARTITION BY grp ORDER BY id) FROM aa ORDER BY id;
-- errors: an unknown argument cannot resolve. PostgreSQL declares array_agg over
-- both anyarray and anynonarray, so an untyped literal fits neither better
SELECT array_agg(NULL);
SELECT array_agg('x');
-- errors: DISTINCT sorts its input, so the type needs an ordering and not just
-- an equality. xid is the one type with a hash opclass and no btree one; this is
-- not array_agg's rule, as the count() line below shows
SELECT array_agg(DISTINCT x) FROM (VALUES ('1'::xid)) t(x);
SELECT count(DISTINCT x) FROM (VALUES ('1'::xid)) t(x);
-- ... while a type with neither operator is refused on the equality first
SELECT array_agg(DISTINCT x) FROM (VALUES ('1'::json)) t(x);
-- an enum has no array type in this build, so array_agg over one is refused
-- (PostgreSQL answers {ok,sad} here.)
CREATE TYPE mood AS ENUM ('sad', 'ok');
SELECT array_agg(x) FROM (VALUES ('ok'::mood), ('sad'::mood)) t(x);
-- the ARRAY[] constructor refuses it the same way, and names the type
-- (PostgreSQL answers {ok} here.)
SELECT ARRAY['ok'::mood];
-- an array argument would need a two-dimensional result, which this build has no
-- representation for
-- (PostgreSQL answers {{10},{NULL},{5},{5},{7}} here.)
SELECT array_agg(ARRAY[val]) FROM aa;
-- End array_agg tests.
