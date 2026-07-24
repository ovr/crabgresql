--
-- COLLATE
-- Collation-driven string ordering: the byte-order collations (C/POSIX/
-- ucs_basic/the database default) versus the locale-aware ICU ones, an explicit
-- COLLATE clause versus a column's declared collation, the errors for a
-- non-collatable type / unknown name / conflicting explicit collations, and the
-- catalog reflection in pg_collation, pg_type, pg_attribute.
--
CREATE TABLE coll_t (a text);
INSERT INTO coll_t VALUES ('apple'), ('Banana'), ('cherry'), ('Apricot'), ('banana');
-- this cluster's database collation is C, so the default is byte order and
-- every uppercase letter sorts before every lowercase one
SELECT a FROM coll_t ORDER BY a;
-- C and POSIX are byte order too, and ucs_basic is code-point order (the same
-- thing in UTF-8)
SELECT a FROM coll_t ORDER BY a COLLATE "C";
SELECT a FROM coll_t ORDER BY a COLLATE "POSIX";
SELECT a FROM coll_t ORDER BY a COLLATE "ucs_basic";
-- an ICU collation ignores case at the primary level: dictionary order
SELECT a FROM coll_t ORDER BY a COLLATE "en-US-x-icu";
-- DESC applies after the collation, not instead of it
SELECT a FROM coll_t ORDER BY a COLLATE "en-US-x-icu" DESC;
-- locale tailoring: Swedish sorts a-ring/a-diaeresis/o-diaeresis after z, while
-- German treats them as variants of the base letter
SELECT s FROM (VALUES ('z'), ('å'), ('a'), ('ä'), ('ö'), ('o')) v(s)
  ORDER BY s COLLATE "sv-x-icu";
SELECT s FROM (VALUES ('z'), ('å'), ('a'), ('ä'), ('ö'), ('o')) v(s)
  ORDER BY s COLLATE "de-x-icu";
-- Czech sorts the digraph "ch" after "h" — here, after "d"
SELECT s FROM (VALUES ('co'), ('ch'), ('cz'), ('d')) v(s) ORDER BY s COLLATE "cs-x-icu";
-- Turkish sorts dotless i before dotted i
SELECT s FROM (VALUES ('i'), ('ı'), ('j'), ('h')) v(s) ORDER BY s COLLATE "tr-x-icu";
-- a collation drives the comparison operators, not just ORDER BY
SELECT a FROM coll_t WHERE a COLLATE "en-US-x-icu" < 'B' ORDER BY a COLLATE "C";
SELECT a FROM coll_t WHERE a < 'B' ORDER BY a COLLATE "C";
-- every supported collation is deterministic, so equality stays byte-exact
SELECT 'A' COLLATE "en-US-x-icu" = 'a' AS eq, 'a' COLLATE "en-US-x-icu" = 'a' AS same;
-- and so grouping is unaffected by the collation
SELECT count(*) AS groups FROM (SELECT a FROM coll_t GROUP BY a) g;
SELECT count(DISTINCT a) AS distinct_vals FROM coll_t;

-- a column may declare its own collation, which applies with no COLLATE clause
CREATE TABLE coll_c (a text COLLATE "de-x-icu", b text);
INSERT INTO coll_c VALUES ('apple', 'apple'), ('Banana', 'Banana'), ('cherry', 'cherry'),
                          ('Apricot', 'Apricot'), ('banana', 'banana');
SELECT a FROM coll_c ORDER BY a;
-- also through a qualified reference and through *
SELECT a FROM coll_c ORDER BY coll_c.a;
SELECT a FROM coll_c ORDER BY a COLLATE "C";
-- a sibling column with no declared collation keeps byte order
SELECT b FROM coll_c ORDER BY b;
-- an explicit clause overrides the column's own collation
SELECT a FROM coll_c ORDER BY a COLLATE "en-US-x-icu";

-- a collation propagates through a function result, as in PG
SELECT lower(a) FROM coll_c ORDER BY lower(a);

-- errors: a non-collatable type, an unknown collation, and two conflicting
-- explicit collations
SELECT 1 COLLATE "C";
SELECT 'x' COLLATE "nope";
SELECT 'a' COLLATE "C" < 'b' COLLATE "POSIX";
CREATE TABLE coll_bad (n int COLLATE "C");
-- an unquoted name folds to lower case, so it does not match "C"
SELECT 'x' COLLATE C;
-- schema-qualifying with pg_catalog is accepted
SELECT 'x' COLLATE pg_catalog."C" AS qualified;

-- catalog reflection
SELECT collname, collprovider, collisdeterministic, collencoding, collcollate, colllocale
  FROM pg_collation WHERE collname IN ('default', 'C', 'POSIX', 'ucs_basic', 'unicode')
  ORDER BY oid;
SELECT typname, typcollation FROM pg_type
  WHERE typname IN ('text', 'varchar', 'bpchar', 'name', 'int4') ORDER BY typname;
-- a column's attcollation is its declared collation, else the type's
SELECT a.attname, a.attcollation = (SELECT oid FROM pg_collation WHERE collname = 'de-x-icu') AS is_de,
       a.attcollation = (SELECT oid FROM pg_collation WHERE collname = 'default') AS is_default
  FROM pg_attribute a
  WHERE a.attrelid = (SELECT oid FROM pg_class WHERE relname = 'coll_c')
  ORDER BY a.attnum;
-- information_schema reports a collation only when it is not the default
SELECT column_name, collation_schema, collation_name FROM information_schema.columns
  WHERE table_name = 'coll_c' ORDER BY column_name;

DROP TABLE coll_c;
DROP TABLE coll_t;
