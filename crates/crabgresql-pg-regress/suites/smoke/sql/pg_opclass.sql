--
-- pg_opclass / pg_opfamily, and the operator class pg_index.indclass names.
--
-- indclass is the reference psql and pg_dump follow to decide whether an index
-- definition has to spell an operator class out. What a client reads through it
-- is the class's *name*, reached by joining pg_opclass and pg_am -- so that is
-- what this file compares. Raw OIDs are pinned only for the classes
-- pg_opclass.dat assigns an OID to itself (int4_ops, text_ops and friends);
-- the rest are numbered by upstream's codegen and move whenever an entry is
-- inserted above them, which is a property of PostgreSQL rather than a
-- regression. Generated with psql -q -a against PostgreSQL 18.4.
--
CREATE TABLE oc_t (i int, t text, v varchar(10));
CREATE INDEX oc_bt ON oc_t (i, t, v);
CREATE INDEX oc_hash ON oc_t USING hash (i);
-- The hand-assigned classes, verbatim. opckeytype is 0 for all of them: a btree
-- stores the indexed type itself.
SELECT oid, opcname, opcintype::regtype, opcdefault, opckeytype
  FROM pg_opclass WHERE oid IN (1978, 1979, 1981, 3122, 3123, 3124, 3125, 3126)
 ORDER BY oid;
-- Every key of the btree index. varchar has no default class of its own: it
-- reaches text_ops through the binary-coercible cast to text, which is why all
-- three keys of oc_bt report the same OID for two different types.
SELECT indclass FROM pg_index WHERE indexrelid = 'oc_bt'::regclass;
-- The same, by name, and with the access method and family each class belongs
-- to -- the whole chain a client follows out of indclass. indclass is an
-- oidvector, whose subscripts are 0-based; it is projected in a subquery
-- because this build does not subscript a table-qualified column yet.
SELECT c.relname, a.amname, oc.opcname, f.opfname, oc.opcintype::regtype
  FROM (SELECT indexrelid, indclass[0] AS class FROM pg_index) i
  JOIN pg_class c ON c.oid = i.indexrelid
  JOIN pg_opclass oc ON oc.oid = i.class
  JOIN pg_opfamily f ON f.oid = oc.opcfamily
  JOIN pg_am a ON a.oid = oc.opcmethod
 WHERE c.relname LIKE 'oc\_%' ORDER BY c.relname;
-- A class is chosen under the index's own access method: the same int column
-- gets int4_ops under either method, but they are two different rows.
SELECT count(DISTINCT oc.oid) AS distinct_classes, count(*) AS keys
  FROM (SELECT indexrelid, indclass[0] AS class FROM pg_index) i
  JOIN pg_class c ON c.oid = i.indexrelid
  JOIN pg_opclass oc ON oc.oid = i.class
 WHERE c.relname LIKE 'oc\_%';
-- Nothing points into thin air: every class names a family, and both name an
-- access method that pg_am publishes.
SELECT count(*) AS dangling_family FROM pg_opclass oc
 WHERE NOT EXISTS (SELECT 1 FROM pg_opfamily f WHERE f.oid = oc.opcfamily);
SELECT count(*) AS dangling_method FROM pg_opclass oc
 WHERE NOT EXISTS (SELECT 1 FROM pg_am a WHERE a.oid = oc.opcmethod);
SELECT count(*) AS dangling_intype FROM pg_opclass oc
 WHERE NOT EXISTS (SELECT 1 FROM pg_type t WHERE t.oid = oc.opcintype);
-- Every class and family lives in pg_catalog, owned by the bootstrap role.
SELECT count(*) AS misplaced FROM pg_opclass
 WHERE opcnamespace <> 'pg_catalog'::regnamespace OR opcowner <> 10;
SELECT count(*) AS misplaced FROM pg_opfamily
 WHERE opfnamespace <> 'pg_catalog'::regnamespace OR opfowner <> 10;
-- A type has one default class per method, never two -- the ambiguity
-- PostgreSQL would have to refuse a CREATE INDEX over.
SELECT count(*) AS ambiguous_defaults FROM (
  SELECT opcmethod, opcintype FROM pg_opclass WHERE opcdefault
   GROUP BY opcmethod, opcintype HAVING count(*) > 1) AS dups;
DROP TABLE oc_t;
