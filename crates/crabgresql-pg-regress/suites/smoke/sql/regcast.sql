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
-- regoper names an operator. Operator OIDs are upstream's own, so unlike the
-- relation OIDs above they are printable here.
SELECT '||/'::regoper AS by_name, 597::regoper AS by_oid;
-- a name is shared by every operand combination it is defined for, so a bare
-- one resolves only when exactly one operator carries it
SELECT '+'::regoper;
-- output applies the same rule from the other side: an operator whose bare name
-- would not read back as itself prints schema-qualified
SELECT 551::regoper AS shared_name, 'pg_catalog.||/'::regoper AS qualified_in;
-- regoper spells the invalid OID `0` where every other reg* type spells it `-`,
-- because `-` is itself an operator name
SELECT 0::regoper AS zero, 999999::regoper AS unknown_oid;
SELECT 'rc_nosuchoperator'::regoper;
-- equality is by OID, and the round trip through oid and text preserves it
SELECT '||/'::regoper = 597::regoper AS same_operator,
       '||/'::regoper::oid AS as_oid, '||/'::regoper::text AS as_text;
-- regoperator names an operator by name *and* operand types, which is exactly
-- what tells the same-named operators above apart: `+` alone is ambiguous,
-- `+(integer,integer)` is one operator
SELECT '+(int4,int4)'::regoperator AS by_name, 551::regoperator AS by_oid;
-- input reads the operands with the type-name grammar, so every spelling of a
-- type works and a typmod is accepted and dropped
SELECT '+(integer,integer)'::regoperator AS sql_spelling,
       'pg_catalog.+(int4,int4)'::regoperator AS qualified_in,
       '=(numeric(10,2),numeric)'::regoperator AS with_typmod;
-- a prefix operator has no left operand, which both halves spell NONE
SELECT '-(NONE,int8)'::regoperator AS by_name, 484::regoperator AS by_oid,
       '||/(none,float8)'::regoperator AS folded;
-- quoted, NONE is an ordinary type name instead
SELECT '=("NONE",int4)'::regoperator;
-- like regoper it spells the invalid OID `0` rather than `-`
SELECT 0::regoperator AS zero, 999999::regoperator AS unknown_oid;
-- equality is by OID, and the round trip through oid and text preserves it
SELECT '+(int4,int4)'::regoperator = 551::regoperator AS same_operator,
       '+(int4,int4)'::regoperator::oid AS as_oid,
       '+(int4,int4)'::regoperator::text AS as_text;
-- the operand types are dropped on the way to regoper, which then has to
-- qualify the name it is left with
SELECT '+(int4,int4)'::regoperator::regoper AS as_regoper;
-- the argument is a signature, not a name: without a parenthesized operand list
-- there is nothing to look up
SELECT '+'::regoperator;
SELECT '+(int4,int4'::regoperator;
SELECT '+(int4,int4) x'::regoperator;
SELECT '=(int4,)'::regoperator;
SELECT '+(int4,int4))'::regoperator;
SELECT '=(,int4)'::regoperator;
-- the name still has to be a name, and is still qualified by the same rules
SELECT '(int4,int4)'::regoperator;
SELECT 'a.b.c.+(int4,int4)'::regoperator;
SELECT 'rc_nosuchdb.pg_catalog.+(int4,int4)'::regoperator;
SELECT (current_database() || '.pg_catalog.+(int4,int4)')::regoperator AS db_qualified;
-- an operator takes two operands, and PostgreSQL counts none as too many
SELECT '+(int4)'::regoperator;
SELECT '+()'::regoperator;
SELECT '+(int4,int4,int4)'::regoperator;
-- the operand types are resolved before they are counted, and a type that does
-- not exist is reported under its parsed spelling
SELECT '=(rc_nosuchtype,int4)'::regoperator;
SELECT 'a.b.c.d.+(RcNoSuchType,int4)'::regoperator;
-- a signature naming no operator echoes the argument exactly as written
SELECT '@#$(int4,int4)'::regoperator;
SELECT '=(none,int4)'::regoperator;
SELECT '"PG_CATALOG".+(int4,int4)'::regoperator;
-- the input function fails softly for pg_input_is_valid, hint and all
SELECT pg_input_is_valid('+(int4,int4)', 'regoperator') AS valid,
       pg_input_is_valid('+(int4)', 'regoperator') AS one_operand;
SELECT message, hint, sql_error_code FROM pg_input_error_info('+(int4)', 'regoperator');
-- regprocedure names a function by its whole signature, which is what makes an
-- overloaded name resolvable — regproc can only take a name no other function
-- carries. Function OIDs are upstream's own, so they are printable here.
SELECT 'int4pl(int4,int4)'::regprocedure AS by_name, 177::regprocedure AS by_oid;
-- output prints the argument types in their SQL spelling, with no space after
-- the comma, so either spelling reads back as the same value
SELECT 'int4pl(integer, integer)'::regprocedure AS sql_spelling,
       'int4pl(int4,int4)'::regprocedure = 'int4pl(integer,integer)'::regprocedure AS eq;
-- space is insignificant everywhere, and an unquoted name folds to lower case
SELECT '  INT4PL ( INT4 , INT4 ) '::regprocedure AS spacey,
       'pg_catalog.int4pl(int4,int4)'::regprocedure AS qualified;
-- a pseudo-type is a legal argument type, and prints as itself
SELECT 'int4in(cstring)'::regprocedure AS pseudo_arg;
-- a function named after a keyword prints quoted, since that is what reads back
-- as the same name
SELECT 1740::regprocedure AS keyword_name;
-- OID 0 renders as a dash (regoper's `0` is the exception, not the rule), and
-- `-` reads back as OID 0 for every reg* type but regoper
SELECT 0::regprocedure AS zero, 999999::regprocedure AS unknown_oid,
       '-'::regprocedure AS dash, '-'::regproc AS dash_proc;
-- the round trip through oid and text preserves the value
SELECT 'int4pl(int4,int4)'::regprocedure::oid AS as_oid,
       'int4pl(int4,int4)'::regprocedure::text AS as_text;
-- a signature no function has is a miss, and the error echoes the argument
-- exactly as written
SELECT 'int4pl(int4,int4,int4)'::regprocedure;
SELECT 'rc_nosuchfunction(int4)'::regprocedure;
-- a bare name is regproc's spelling, not this one
SELECT 'int4pl'::regprocedure;
SELECT 'int4pl(int4'::regprocedure;
-- the argument list has its own grammar, and each way of getting it wrong
-- reports differently. Bracketing that does not match is settled before the
-- type grammar sees anything, which is why it alone says `improper type name`
SELECT 'int4pl(int4))'::regprocedure;
SELECT 'int4pl(int4])'::regprocedure;
SELECT 'int4pl(int4[)'::regprocedure;
SELECT 'int4pl(int4,)'::regprocedure;
SELECT 'int4pl(,int4)'::regprocedure;
SELECT '(int4)'::regprocedure;
-- ... while a spelling the type grammar rejects reports the token it stopped on
SELECT 'int4pl(int4 int4)'::regprocedure;
SELECT 'int4pl(4)'::regprocedure;
SELECT 'int4pl(.int4)'::regprocedure;
SELECT 'int4pl(int4.)'::regprocedure;
SELECT 'int4pl(int4[1,2])'::regprocedure;
-- an argument list longer than PG's FUNC_MAX_ARGS is a limit, not a syntax
-- error, and is the one complaint here that is not 22P02
SELECT ('int4pl(' || repeat('int,', 100) || 'int)')::regprocedure;
-- an argument type is resolved before the function is looked up, so a type that
-- does not exist is reported as such — and a missing schema before that
SELECT 'int4pl(rc_nosuchtype)'::regprocedure;
SELECT 'int4pl(rc_nosuchschema.t)'::regprocedure;
-- and an argument type's own name is qualified by the same rules as any other
SELECT 'int4pl(a.b.c.d)'::regprocedure;
SELECT 'int4pl(a.b.c)'::regprocedure;
-- the argument types are read before the *function* name is deconstructed, so a
-- four-part name is reported only once the types resolve
SELECT 'a.b.c.d(rc_nosuchtype)'::regprocedure;
SELECT 'a.b.c.d(int4)'::regprocedure;
SELECT 'a.b.c(int4)'::regprocedure;
-- ... but the name's own *syntax* is checked before either
SELECT 'a b(rc_nosuchtype)'::regprocedure;
-- a session's own functions resolve the same way, including a zero-argument one
-- and a name that has to be quoted to read back
CREATE FUNCTION rc_f(a int, b text) RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION rc_z() RETURNS int LANGUAGE sql AS 'SELECT 1';
CREATE FUNCTION "rc Mixed"(a int) RETURNS int LANGUAGE sql AS 'SELECT 1';
SELECT 'rc_f(int4,text)'::regprocedure AS user_fn,
       'public.rc_f(int,text)'::regprocedure AS qualified_user_fn;
SELECT 'rc_z()'::regprocedure AS zero_args, '"rc Mixed"(int)'::regprocedure AS quoted;
SELECT 'rc_f(int4,text)'::regprocedure::oid::regprocedure AS roundtrip;
SELECT 'rc_f(int4)'::regprocedure;
DROP FUNCTION rc_f(int, text);
DROP FUNCTION rc_z();
DROP FUNCTION "rc Mixed"(int);
--
-- reg* NAME PARSING
-- a built-in whose SQL spelling is several words is one *type name*, not
-- several identifiers, and regtype reads it as such
SELECT 'character varying'::regtype AS vc, 'double precision'::regtype AS f8,
       'timestamp with time zone'::regtype AS tstz;
-- a string that does not parse as a name at all is a syntax error, not a miss
SELECT ''::regclass;
SELECT '"unterminated'::regclass;
SELECT 'rc_t.'::regclass;
SELECT 'rc t'::regclass;
-- ... but an explicitly quoted empty part is a name, and merely names nothing
SELECT '""'::regclass;
-- a three-part name carries a database: accepted when it names the one this
-- session is connected to, and rejected otherwise
SELECT (current_database() || '.public.rc_t')::regclass AS db_qualified;
SELECT 'rc_nosuchdb.public.rc_t'::regclass;
-- the wording is per kind, and past three parts nothing qualifies a name at all
SELECT 'a.b.c.d'::regclass;
SELECT 'a.b.c'::regproc;
SELECT 'a.b.c.d'::regtype;
-- a schema name is never qualified at all, which makes a dotted one a syntax
-- error where the same string would be a plain miss for the other kinds
SELECT 'a.b'::regnamespace;
-- the "does not exist" text echoes the *parsed* name, except for regproc and
-- regoper, which echo the argument exactly as written
SELECT 'PUB.RcNoSuch'::regclass;
SELECT 'PUB.RcNoSuch'::regproc;
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
-- an array of a reg* type is named for its element too, under every spelling
SELECT 2209::regtype AS by_oid, '_regoperator'::regtype AS typname_spelling,
       'regoperator[]'::regtype AS rendered_spelling;
SELECT 2210::regtype AS regclass_array, 1008::regtype AS regproc_array;
