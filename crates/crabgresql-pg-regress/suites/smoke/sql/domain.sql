--
-- CREATE DOMAIN / ALTER DOMAIN / DROP DOMAIN
-- A domain is a distinct type over a base type: values are physically the
-- base's, but the domain is what pg_typeof and pg_attribute name, and its
-- constraints run wherever a value enters it — an explicit cast included.
-- Expected output generated from PostgreSQL 18.4 with `psql -q -a`.
--
CREATE DOMAIN posint AS int CHECK (VALUE > 0);
SELECT 1::posint;
SELECT (-1)::posint;
-- A NULL passes a CHECK, as it does for a table constraint.
SELECT NULL::posint;
-- The declared type is the domain, not the base.
SELECT pg_typeof(1::posint);
-- NOT NULL is a constraint of its own, with its own SQLSTATE.
CREATE DOMAIN nn AS int NOT NULL;
SELECT NULL::nn;
SELECT 1::nn;
-- A domain carries the type modifier; a column of it has atttypmod -1.
CREATE DOMAIN v3 AS varchar(3);
SELECT 'ab'::v3;
SELECT 'abcd'::v3;
--
-- As a column type.
--
CREATE TABLE t (a posint, b v3);
INSERT INTO t VALUES (1, 'ab');
INSERT INTO t VALUES (-1, 'ab');
INSERT INTO t VALUES (1, 'abcd');
UPDATE t SET a = -5;
-- The column reports the domain; anything computed from it reports the base.
SELECT a, b, a + 1, pg_typeof(a), pg_typeof(a + 1) FROM t;
SELECT atttypid::regtype, atttypmod FROM pg_attribute WHERE attrelid = 't'::regclass AND attnum > 0 ORDER BY attnum;
--
-- A domain over a domain: the inner (base) constraints run first.
--
CREATE DOMAIN dd AS posint CHECK (VALUE > 10);
SELECT 50::dd;
SELECT 5::dd;
SELECT (-5)::dd;
SELECT typbasetype::regtype FROM pg_type WHERE typname = 'dd';
--
-- Two violated constraints resolve by name, not by declaration order.
--
CREATE DOMAIN ord AS int CONSTRAINT bbb CHECK (VALUE > 100) CONSTRAINT aaa CHECK (VALUE > 200);
SELECT 0::ord;
--
-- A domain default fills a column that has none of its own, and is resolved
-- per statement rather than copied into the table.
--
CREATE DOMAIN dz AS int DEFAULT 7;
CREATE TABLE tz (x dz, y int);
INSERT INTO tz(y) VALUES (1);
ALTER DOMAIN dz SET DEFAULT 9;
INSERT INTO tz(y) VALUES (2);
SELECT * FROM tz ORDER BY y;
SELECT column_default FROM information_schema.columns WHERE table_name = 'tz' AND column_name = 'x';
--
-- ALTER DOMAIN re-scans the columns already typed on the domain.
--
CREATE DOMAIN d AS int;
CREATE TABLE tt (x d);
INSERT INTO tt VALUES (1), (NULL), (-3);
ALTER DOMAIN d SET NOT NULL;
ALTER DOMAIN d ADD CONSTRAINT c1 CHECK (VALUE > 0);
-- NOT VALID skips the scan; VALIDATE runs it.
ALTER DOMAIN d ADD CONSTRAINT c1 CHECK (VALUE > 0) NOT VALID;
ALTER DOMAIN d VALIDATE CONSTRAINT c1;
DELETE FROM tt WHERE x IS NULL OR x < 0;
ALTER DOMAIN d SET NOT NULL;
ALTER DOMAIN d VALIDATE CONSTRAINT c1;
ALTER DOMAIN d ADD CHECK (VALUE < 100);
INSERT INTO tt VALUES (0);
INSERT INTO tt VALUES (500);
ALTER DOMAIN d DROP CONSTRAINT c1;
ALTER DOMAIN d DROP CONSTRAINT c1;
ALTER DOMAIN d DROP CONSTRAINT IF EXISTS c1;
ALTER DOMAIN d RENAME CONSTRAINT d_check TO zz;
ALTER DOMAIN d DROP NOT NULL;
ALTER DOMAIN d SET DEFAULT 3;
ALTER DOMAIN d DROP DEFAULT;
ALTER DOMAIN nosuch SET NOT NULL;
--
-- Operators, functions and the type-unifying constructs all resolve on the
-- base — except where every branch is the *same* domain, which keeps it.
--
CREATE TABLE u (x posint);
INSERT INTO u VALUES (1), (2), (3);
SELECT max(x), min(x), sum(x), count(x) FROM u;
SELECT abs(x), x * 2 FROM u ORDER BY 1;
SELECT array_agg(x ORDER BY x) FROM u;
SELECT x, count(*) FROM u GROUP BY x ORDER BY x;
SELECT DISTINCT x FROM u ORDER BY x;
SELECT pg_typeof(CASE WHEN true THEN x ELSE x END) FROM u LIMIT 1;
SELECT pg_typeof(CASE WHEN true THEN x ELSE 0 END) FROM u LIMIT 1;
SELECT pg_typeof(coalesce(x, x)), pg_typeof(greatest(x, x)) FROM u LIMIT 1;
SELECT pg_typeof(coalesce(x, 0)), pg_typeof(greatest(x, 0)) FROM u LIMIT 1;
-- An *untyped* branch counts as a different type: PG's `select_common_type`
-- takes its "every input is already this type" fast path only when it really
-- is, and the full algorithm it falls into opens with `getBaseType`. So a bare
-- NULL, or a literal that has to take its type from the set, resolves the whole
-- thing on the base — where reading the literal as the domain would have failed
-- outright for a text domain.
SELECT pg_typeof(coalesce(x, NULL)), pg_typeof(greatest(x, NULL)) FROM u LIMIT 1;
SELECT pg_typeof(CASE WHEN true THEN x ELSE NULL END) FROM u LIMIT 1;
CREATE DOMAIN sident AS name;
CREATE TABLE us (x sident);
INSERT INTO us VALUES ('a');
SELECT coalesce(x, 'z'), greatest(x, 'z'), CASE WHEN false THEN x ELSE 'z' END FROM us;
SELECT pg_typeof(coalesce(x, 'z')) FROM us;
DROP TABLE us;
DROP DOMAIN sident;
SELECT pg_typeof(y) FROM (SELECT x AS y FROM u UNION SELECT x FROM u) s LIMIT 1;
SELECT pg_typeof(y) FROM (SELECT x AS y FROM u UNION SELECT 9) s LIMIT 1;
DROP TABLE u;
--
-- A domain on the *source* side of an assignment is just its base value.
--
CREATE TABLE plain (x int);
INSERT INTO plain(x) SELECT a FROM t;
SELECT * FROM plain;
UPDATE plain SET x = (SELECT a FROM t);
DROP TABLE plain;
--
-- A CHECK is evaluated for NULL too — only a `false` violates. That is what
-- makes `CHECK (VALUE IS NOT NULL)`, the spelling the PostgreSQL manual
-- suggests, actually reject a NULL while `CHECK (VALUE > 0)` admits one.
--
CREATE DOMAIN notnullish AS int CHECK (VALUE IS NOT NULL);
SELECT NULL::notnullish;
SELECT 1::notnullish;
--
-- A domain over a domain inherits the modifier its chain declares, even though
-- its own typtypmod is -1.
--
CREATE DOMAIN v3b AS v3;
SELECT 'abcd'::v3b;
SELECT typtypmod FROM pg_type WHERE typname = 'v3b';
--
-- COPY parses through the base type and still enforces the domain.
--
-- One documented divergence in the expected output: PostgreSQL follows a failed
-- COPY with a `CONTEXT: COPY loaded, line 1, column a: "-1"` line. This build
-- carries no error context on any COPY error, so those lines are absent — a gap
-- in COPY diagnostics generally, not in the domain check that raised them.
--
CREATE TABLE loaded (a posint, b v3, c int);
COPY loaded (a, b, c) FROM stdin;
5	ab	1
\.
COPY loaded (a, b, c) FROM stdin;
-1	ab	2
\.
COPY loaded (a, b, c) FROM stdin;
7	abcd	3
\.
COPY loaded (a, b, c) FROM stdin;
\N	xy	4
\.
SELECT * FROM loaded ORDER BY c;
DROP TABLE loaded;
--
-- A domain constraint runs before the relation's own NOT NULL: it belongs to
-- the coercion, which happens before the row exists.
--
CREATE TABLE ordering (a posint NOT NULL, b int NOT NULL);
INSERT INTO ordering VALUES (-1, NULL);
DROP TABLE ordering;
--
-- A domain as a function's parameter and as its return type.
--
CREATE FUNCTION takes(p posint) RETURNS int AS 'SELECT p::int * 2' LANGUAGE SQL;
SELECT takes(5);
SELECT takes(-1);
CREATE FUNCTION returns_dom(p int) RETURNS posint AS 'SELECT p' LANGUAGE SQL;
SELECT returns_dom(5);
SELECT returns_dom(-1);
--
-- A domain is where a collation can be declared, and it drives the ordering.
-- `y` pins `C` explicitly so the comparison is against a known order rather
-- than against whatever collation the database was created with.
--
CREATE DOMAIN ci AS text COLLATE "en-x-icu";
CREATE DOMAIN dtext AS text;
CREATE TABLE collated (x ci, y text COLLATE "C", z dtext);
INSERT INTO collated VALUES ('B', 'B', 'B'), ('a', 'a', 'a'), ('A', 'A', 'A'), ('b', 'b', 'b');
SELECT x FROM collated ORDER BY x;
SELECT y FROM collated ORDER BY y;
SELECT z FROM collated ORDER BY z COLLATE "en-x-icu";
DROP TABLE collated;
--
-- Catalog reflection.
--
SELECT typname, typtype, typlen, typbyval, typcategory, typinput, typoutput, typalign, typstorage, typnotnull, typbasetype::regtype, typtypmod, typcollation, typdefault
  FROM pg_type WHERE typname IN ('posint', 'nn', 'v3', 'dz') ORDER BY typname;
-- Likewise scoped: upstream's information_schema domains carry constraints of
-- their own.
--
-- One documented divergence in the expected output below: PostgreSQL renders
-- `dd_check` as `CHECK (((VALUE)::integer > 10))`, decorating VALUE with the
-- coercion out of the base *domain* `posint`. It stores a node tree and can see
-- that coercion; this build stores the predicate as SQL text, where there is no
-- cast to render. The predicate itself is the same one, and both agree on every
-- value.
SELECT conname, contype, contypid::regtype, conrelid, conkey, convalidated, pg_get_constraintdef(oid)
  FROM pg_constraint c WHERE contypid <> 0
   AND (SELECT typnamespace FROM pg_type WHERE oid = c.contypid) = 'public'::regnamespace
 ORDER BY conname;
-- A domain constraint constrains a *type*, so its dependency lands on the
-- domain and not on a relation it does not have. Scoped to `public` to keep
-- information_schema's own two out of the picture, which the same rule covers.
SELECT c.conname, d.refclassid::regclass AS refclass, d.refobjid::regtype AS refobj, d.deptype
  FROM pg_depend d
  JOIN pg_constraint c ON c.oid = d.objid
 WHERE d.classid = 'pg_constraint'::regclass
   AND c.contypid <> 0
   AND (SELECT typnamespace FROM pg_type WHERE oid = c.contypid) = 'public'::regnamespace
 ORDER BY c.conname;
-- Scoped to `public`: PostgreSQL's own information_schema is built on five
-- domains of its own, which `information_schema_domains` covers.
SELECT domain_name, data_type, character_maximum_length, numeric_precision, domain_default, udt_schema, udt_name, dtd_identifier
  FROM information_schema.domains WHERE domain_schema = 'public' ORDER BY domain_name;
SELECT column_name, data_type, character_maximum_length, domain_schema, domain_name, udt_name
  FROM information_schema.columns WHERE table_name = 't' ORDER BY ordinal_position;
--
-- DROP.
--
DROP DOMAIN d;
DROP TABLE tt;
DROP DOMAIN d;
DROP DOMAIN d;
DROP DOMAIN IF EXISTS d;
DROP DOMAIN posint;
DROP TABLE t;
DROP TABLE tz;
DROP DOMAIN dd;
DROP DOMAIN posint;
DROP DOMAIN nn, dz, ord, notnullish, ci, dtext;
DROP FUNCTION takes(posint);
DROP FUNCTION returns_dom(int);
DROP DOMAIN v3b;
DROP DOMAIN v3;
