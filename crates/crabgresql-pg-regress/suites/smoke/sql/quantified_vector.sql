--
-- ANY/ALL over int2vector / oidvector, and the two operator errors
--
-- PostgreSQL's `x op ANY (y)` accepts any y whose type has a typelem, not just
-- a real array type -- which is why the catalog idiom `a.attnum = ANY(i.indkey)`
-- works even though indkey is int2vector. This build keeps int2vector and
-- oidvector out of the array family (ARRAY[v] builds oidvector[], it does not
-- flatten), so the quantified path reaches their element type through the
-- vector's own element rule instead.
--
-- The errors at the end are PostgreSQL's two ANY/ALL rejections, in the order
-- it checks them: the right side must be an array *before* the operator is
-- judged, so `33 * any (44)` blames the right side and `33 * any ('{1,2,3}')`
-- blames the operator. Both ported from
-- vendor/postgres/regress/sql/arrays.sql and matching PostgreSQL 18.4 byte for
-- byte, cursor included.
--

-- A vector on the right side: elements are compared like array elements.
SELECT 1 = ANY('1 2'::int2vector) AS found,
       3 = ANY('1 2'::int2vector) AS missing,
       5 = ALL('1 2'::int2vector) AS all_less,
       1 > ALL('2 3'::int2vector) AS gt_all;
SELECT 1::oid = ANY('1 2'::oidvector) AS found,
       3::oid = ANY('1 2'::oidvector) AS missing,
       0::oid < ALL('1 2'::oidvector) AS lt_all;
-- An empty vector is the quantifier's identity, a NULL vector is NULL.
SELECT 1 = ANY(''::int2vector) AS empty_any,
       1 = ALL(''::int2vector) AS empty_all,
       1 = ANY(NULL::int2vector) AS null_any,
       1 = ALL(NULL::int2vector) AS null_all;
-- The needle keeps its own type: int4 and numeric compare against int2 elements
-- after the usual promotion, so 1.5 matches nothing rather than rounding.
SELECT 1 = ANY('1 2'::int2vector) AS int4_needle,
       1.5 = ANY('1 2'::int2vector) AS numeric_needle,
       2::int8 = ANY('1 2'::int2vector) AS int8_needle;
-- NULL needle is NULL, as for an array.
SELECT NULL::int = ANY('1 2'::int2vector) AS null_needle;

-- Through a table column.
CREATE TABLE qvec (id int, v int2vector, o oidvector);
INSERT INTO qvec VALUES (1, '1 2', '11 22'), (2, '3', '33');
SELECT id FROM qvec WHERE 2 = ANY(v) ORDER BY id;
SELECT id FROM qvec WHERE 22::oid = ANY(o) ORDER BY id;
SELECT id FROM qvec WHERE 9 <> ALL(v) ORDER BY id;
DROP TABLE qvec;

-- The catalog idiom this exists for: which columns an index covers.
CREATE TABLE qvidx (a int, b int, c int, PRIMARY KEY (b, a));
SELECT a.attname
  FROM pg_attribute a
  JOIN pg_index i ON i.indrelid = a.attrelid
 WHERE i.indrelid = 'qvidx'::regclass AND a.attnum = ANY(i.indkey)
 ORDER BY a.attname;
DROP TABLE qvidx;

-- Errors. The right side is judged first...
SELECT 33 * any (44);
SELECT 1 = any (1 << 2);
-- ...and only then the operator's result type.
SELECT 33 * any ('{1,2,3}');
SELECT 'a' || any (array['b']);
-- The qualified operator spelling psql and pg_dump write is the same operator.
SELECT 1 OPERATOR(pg_catalog.=) ANY (ARRAY[1,2]) AS qualified;
