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
-- sequences are relations, so nextval takes either spelling
CREATE SEQUENCE rc_s;
SELECT nextval('rc_s') AS bare, nextval('rc_s'::regclass) AS via_regclass;
SELECT setval('rc_s'::regclass, 10) AS after_setval;
DROP SEQUENCE rc_s;
DROP TABLE rc_t;
DROP TABLE rcs.rc_u;
DROP SCHEMA rcs;
