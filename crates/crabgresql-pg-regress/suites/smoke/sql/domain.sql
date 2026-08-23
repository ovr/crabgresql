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
SELECT pg_typeof(y) FROM (SELECT x AS y FROM u UNION SELECT x FROM u) s LIMIT 1;
SELECT pg_typeof(y) FROM (SELECT x AS y FROM u UNION SELECT 9) s LIMIT 1;
DROP TABLE u;
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
-- Scoped to `public`: PostgreSQL's own information_schema is itself built on
-- domains (sql_identifier, cardinal_number, …), which this build has none of.
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
DROP DOMAIN nn, v3, dz, ord;
