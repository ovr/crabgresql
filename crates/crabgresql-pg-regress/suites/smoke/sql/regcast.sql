--
-- reg* CASTS
-- regclass/regtype/regnamespace store an OID and render as the name of what
-- they identify. psql's \d leans on this (`conrelid::regclass`,
-- `c.oid::pg_catalog.regclass::pg_catalog.text`, `reloftype::regtype`).
--
-- No query here prints a relation OID: crabgresql assigns synthetic ones, so
-- only the *names* they render as are comparable with PostgreSQL.
CREATE TABLE rc_t (a integer);
CREATE SCHEMA rcs;
CREATE TABLE rcs.rc_u (a integer);
-- a name resolves to the relation, and renders back as the same name
SELECT 'rc_t'::regclass AS by_name;
-- an unquoted name folds to lower case, and surrounding space is ignored
SELECT 'RC_T'::regclass AS folded, '  rc_t  '::regclass AS trimmed;
-- a relation an unqualified name does not reach renders schema-qualified
SELECT 'rcs.rc_u'::regclass AS qualified;
-- OID 0 renders as a dash, and an OID naming nothing renders as its digits
SELECT 0::regclass AS zero, 4294967000::regclass AS unknown_oid;
-- regtype renders a builtin under its SQL spelling, not its catalog one
SELECT 25::regtype AS text_t, 23::regtype AS int_t, 1043::regtype AS varchar_t;
-- including array types
SELECT 1005::regtype AS int2_array;
-- and accepts either spelling on input
SELECT 'integer'::regtype AS sql_spelling, 'int4'::regtype AS catalog_spelling;
SELECT 0::regtype AS zero_type;
-- regnamespace names a schema
SELECT 'rcs'::regnamespace AS by_name, 'public'::regnamespace AS pub;
-- casting to text goes through the rendered name, not the OID
SELECT 'rc_t'::regclass::text AS as_text, length('rc_t'::regclass::text) AS len;
-- the round trip through oid preserves the value
SELECT 'rc_t'::regclass::oid = 'rc_t'::regclass::oid AS oid_roundtrip;
-- equality is by OID, so two spellings of one relation are equal
SELECT 'rc_t'::regclass = 'RC_T'::regclass AS same_relation;
SELECT 'rcs.rc_u'::regclass = 'rc_t'::regclass AS different_relations;
-- a name that resolves to nothing is an error, per kind of object
SELECT 'rc_nosuchtable'::regclass;
SELECT 'rc_nosuchtype'::regtype;
SELECT 'rc_nosuchschema'::regnamespace;
-- pg_typeof reports its argument's type as a regtype. It is polymorphic, so it
-- has no fixed signature; the argument is never evaluated, only its type read.
SELECT pg_typeof(1) AS int_, pg_typeof(1.5) AS num, pg_typeof(true) AS bool_,
       pg_typeof('a'::text) AS text_;
-- the SQL spelling, as everywhere else in regtype
SELECT pg_typeof('2020-01-01'::timestamptz) AS tstz, pg_typeof('x'::varchar) AS vc;
-- a regtype is only an OID, so the type modifier is not reported
SELECT pg_typeof(1::numeric(10,2)) AS numeric_mod, pg_typeof('x'::varchar(5)) AS varchar_mod;
-- an array reports the element type's array spelling
SELECT pg_typeof(ARRAY[1,2]) AS arr;
-- a literal that never acquired a type really is `unknown`, not text
SELECT pg_typeof('abc') AS unknown_lit, pg_typeof(NULL) AS null_lit;
SELECT 705::regtype AS unknown_by_oid;
-- the result is an ordinary regtype: comparable, castable, and self-describing
SELECT pg_typeof(1) = 'integer'::regtype AS eq, pg_typeof(1)::text AS as_text,
       pg_typeof(pg_typeof(1)) AS selfref;
-- a column's declared type, which is the usual reason to call this
CREATE TABLE pt_t (a integer, b timestamptz, c numeric(8,3));
INSERT INTO pt_t VALUES (1, '2020-01-01+00', 1.5);
SELECT pg_typeof(a) AS a, pg_typeof(b) AS b, pg_typeof(c) AS c FROM pt_t;
DROP TABLE pt_t;
-- pg_typeof takes exactly one argument
SELECT pg_typeof(1, 2);
-- pseudo-types (pg_type.typtype = 'p') have a catalog row but no runtime
-- representation, so they are named from a shared table rather than resolved as
-- a type. `any` prints quoted, and oid 2287's typname `_record` prints as an array.
SELECT 705::regtype AS unknown_, 2249::regtype AS record_, 2276::regtype AS any_,
       2283::regtype AS anyelement_, 2287::regtype AS record_array;
-- the input direction resolves the same names, and folds case like any other
SELECT 'unknown'::regtype AS u, 'record'::regtype AS r, '"any"'::regtype AS a,
       'ANYELEMENT'::regtype AS folded, 'pg_catalog.void'::regtype AS qualified;
SELECT 'unknown'::regtype = 705::regtype AS roundtrip;
-- format_type and regtype must agree on the name
SELECT format_type(705, NULL) AS ft_unknown, format_type(2249, NULL) AS ft_record;
-- pg_typeof reports an untyped literal as `unknown`, which is why the above
-- matters; the two spellings compare equal
SELECT pg_typeof('abc') AS lit, pg_typeof('abc') = 'unknown'::regtype AS eq;
-- pg_typeof keeps its argument: the argument is still evaluated, and every pass
-- that walks a call's arguments still sees it.
CREATE TABLE pt_ev (a int, b int);
INSERT INTO pt_ev VALUES (1, 10), (2, 20), (3, 30);
-- an aggregate inside pg_typeof still groups, so this is one row and not three
SELECT pg_typeof(count(*)) AS agg FROM pt_ev;
-- ... and a bare column beside a grouped one is still rejected
SELECT pg_typeof(a) FROM pt_ev GROUP BY b;
-- the argument's own errors still surface
SELECT pg_typeof(1/0);
-- ... and its side effects still happen
CREATE SEQUENCE pt_s;
SELECT pg_typeof(nextval('pt_s')) AS nx;
SELECT currval('pt_s') AS advanced;
-- a stored DEFAULT round-trips: the argument keeps the type it was written with,
-- so a bare literal is still `unknown` when the default is re-bound per row
CREATE TABLE pt_def (b text DEFAULT pg_typeof('abc'), c text DEFAULT pg_typeof(1),
                     d text DEFAULT pg_typeof('x'::text));
SELECT column_name, column_default FROM information_schema.columns
 WHERE table_name = 'pt_def' ORDER BY ordinal_position;
INSERT INTO pt_def DEFAULT VALUES;
SELECT * FROM pt_def;
DROP TABLE pt_def;
DROP SEQUENCE pt_s;
DROP TABLE pt_ev;
-- sequences are relations, so nextval takes either spelling
CREATE SEQUENCE rc_s;
SELECT nextval('rc_s') AS bare, nextval('rc_s'::regclass) AS via_regclass;
SELECT setval('rc_s'::regclass, 10) AS after_setval;
DROP SEQUENCE rc_s;
DROP TABLE rc_t;
DROP TABLE rcs.rc_u;
DROP SCHEMA rcs;
-- the array pseudo-type resolves under both spellings, so what regtype prints is
-- always something regtype can read back
SELECT '_record'::regtype AS typname_spelling, 'record[]'::regtype AS rendered_spelling;
SELECT 2287::regtype::text::regtype AS roundtrip;
