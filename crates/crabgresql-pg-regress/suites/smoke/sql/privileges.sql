--
-- PRIVILEGES
-- pg_has_role and the nine has_*_privilege families: what they answer, how they
-- read their arguments, and which error each malformed call raises.
--
-- Nothing here prints the login role's name or an OID, for the reason the roles
-- suite gives: the expected output is generated against a PostgreSQL whose
-- bootstrap superuser is whatever its initdb chose. Every object below is
-- created by the session, so the session role owns it in both servers, and the
-- roles the suite creates own nothing.
--
CREATE TABLE priv_t (a int, b text);
CREATE SEQUENCE priv_s;
CREATE SCHEMA priv_sch;
CREATE ROLE priv_plain;
CREATE ROLE priv_group;
CREATE ROLE priv_member;
CREATE ROLE priv_noinherit NOINHERIT;
GRANT priv_group TO priv_member;
GRANT priv_group TO priv_noinherit;
-- The owner holds every privilege on its own objects, grant option included.
SELECT has_table_privilege('priv_t', 'SELECT') AS sel,
       has_table_privilege('priv_t', 'INSERT, UPDATE, DELETE') AS write,
       has_table_privilege('priv_t', 'TRUNCATE') AS trunc,
       has_table_privilege('priv_t', 'REFERENCES') AS refs,
       has_table_privilege('priv_t', 'TRIGGER') AS trig,
       has_table_privilege('priv_t', 'MAINTAIN') AS maint,
       has_table_privilege('priv_t', 'SELECT WITH GRANT OPTION') AS grantable;
SELECT has_column_privilege('priv_t', 'a', 'SELECT') AS col,
       has_any_column_privilege('priv_t', 'SELECT') AS any_col,
       has_sequence_privilege('priv_s', 'SELECT, UPDATE, USAGE') AS seq,
       has_schema_privilege('priv_sch', 'CREATE, USAGE') AS sch;
-- A role that owns nothing and is a member of nothing holds only what the
-- default ACL leaves to PUBLIC: USAGE on a type, EXECUTE on a function.
SELECT has_table_privilege('priv_plain', 'priv_t', 'SELECT') AS tab,
       has_column_privilege('priv_plain', 'priv_t', 'a', 'SELECT') AS col,
       has_any_column_privilege('priv_plain', 'priv_t', 'SELECT') AS any_col,
       has_sequence_privilege('priv_plain', 'priv_s', 'USAGE') AS seq,
       has_schema_privilege('priv_plain', 'priv_sch', 'USAGE') AS sch,
       has_type_privilege('priv_plain', 'int4', 'USAGE') AS typ,
       has_function_privilege('priv_plain', 'int4pl(int4,int4)', 'EXECUTE') AS fun;
-- ... and never with the grant option, which only an owner has to give.
SELECT has_type_privilege('priv_plain', 'int4', 'USAGE WITH GRANT OPTION') AS typ,
       has_function_privilege('priv_plain', 'int4pl(int4,int4)',
                              'EXECUTE WITH GRANT OPTION') AS fun;
-- A system catalog and the pg_catalog schema are the exception: initdb opens
-- them to PUBLIC.
SELECT has_table_privilege('priv_plain', 'pg_class', 'SELECT') AS cat_select,
       has_table_privilege('priv_plain', 'pg_class', 'INSERT') AS cat_insert,
       has_schema_privilege('priv_plain', 'pg_catalog', 'USAGE') AS cat_usage;
-- ... but not every catalog: the ones holding password verifiers, planner
-- statistics or connection strings are closed to PUBLIC.
SELECT has_table_privilege('priv_plain', 'pg_authid', 'SELECT') AS authid,
       has_table_privilege('priv_plain', 'pg_shadow', 'SELECT') AS shadow,
       has_table_privilege('priv_plain', 'pg_statistic', 'SELECT') AS stats,
       has_table_privilege('priv_plain', 'pg_subscription', 'SELECT') AS sub,
       has_table_privilege('priv_plain', 'pg_type', 'SELECT') AS typ;
-- The public schema carries a USAGE grant to PUBLIC and nothing more: CREATE
-- there stayed with the owner in PostgreSQL 15.
SELECT has_schema_privilege('priv_plain', 'public', 'USAGE') AS usage,
       has_schema_privilege('priv_plain', 'public', 'CREATE') AS create_,
       has_schema_privilege('priv_plain', 'public',
                            'USAGE WITH GRANT OPTION') AS grantable;
-- A list is held when *any* of its privileges is.
SELECT has_table_privilege('priv_plain', 'pg_class',
                           'INSERT, UPDATE, SELECT') AS any_of,
       has_table_privilege('priv_plain', 'pg_class', 'INSERT, UPDATE') AS none_of;
-- Privilege names are case-insensitive and may carry surrounding blanks.
SELECT has_table_privilege('priv_t', 'select') AS lower,
       has_table_privilege('priv_t', '  SELECT ,  INSERT  ') AS padded;
-- pg_has_role: MEMBER is membership at all, USAGE is membership whose
-- privileges arrive without a SET ROLE, SET is being allowed to SET ROLE to it.
SELECT pg_has_role('priv_member', 'priv_group', 'MEMBER') AS member,
       pg_has_role('priv_member', 'priv_group', 'USAGE') AS usage,
       pg_has_role('priv_member', 'priv_group', 'SET') AS set,
       pg_has_role('priv_member', 'priv_group',
                   'MEMBER WITH ADMIN OPTION') AS admin;
-- A NOINHERIT member is a member and may SET ROLE, but inherits nothing.
SELECT pg_has_role('priv_noinherit', 'priv_group', 'MEMBER') AS member,
       pg_has_role('priv_noinherit', 'priv_group', 'USAGE') AS usage,
       pg_has_role('priv_noinherit', 'priv_group', 'SET') AS set;
-- A role holds every membership privilege over itself, but no admin option.
SELECT pg_has_role('priv_plain', 'priv_plain', 'USAGE') AS usage,
       pg_has_role('priv_plain', 'priv_plain', 'SET') AS set,
       pg_has_role('priv_plain', 'priv_plain',
                   'USAGE WITH ADMIN OPTION') AS admin;
-- A role is not a member of an unrelated one, in either direction.
SELECT pg_has_role('priv_plain', 'priv_group', 'MEMBER') AS plain_of_group,
       pg_has_role('priv_group', 'priv_member', 'MEMBER') AS group_of_member;
-- The membership privileges are inherited through a member's own objects too:
-- a member of the owner holds what the owner holds.
GRANT priv_member TO priv_plain;
SELECT pg_has_role('priv_plain', 'priv_group', 'USAGE') AS transitive;
REVOKE priv_member FROM priv_plain;
-- The admin option travels differently from the rest: it is held by anyone who
-- can reach a role that was granted it, and the reaching is plain membership —
-- neither the INHERIT option of the step nor an admin option on the step
-- itself matters.
CREATE ROLE priv_admin;
CREATE ROLE priv_indirect;
CREATE ROLE priv_indirect_noinherit NOINHERIT;
GRANT priv_group TO priv_admin WITH ADMIN OPTION;
GRANT priv_admin TO priv_indirect;
GRANT priv_admin TO priv_indirect_noinherit;
SELECT pg_has_role('priv_admin', 'priv_group',
                   'USAGE WITH ADMIN OPTION') AS direct,
       pg_has_role('priv_indirect', 'priv_group',
                   'USAGE WITH ADMIN OPTION') AS through_a_member,
       pg_has_role('priv_indirect_noinherit', 'priv_group',
                   'MEMBER WITH ADMIN OPTION') AS through_noinherit,
       pg_has_role('priv_indirect', 'priv_admin',
                   'USAGE WITH ADMIN OPTION') AS on_the_step_itself,
       pg_has_role('priv_member', 'priv_group',
                   'USAGE WITH ADMIN OPTION') AS unrelated_member;
--
-- How each family reads a name.
--
-- A relation name is an identifier: case-folded, quotable, qualifiable, and
-- never an OID in disguise.
SELECT has_table_privilege('PRIV_T', 'SELECT') AS folded,
       has_table_privilege('"priv_t"', 'SELECT') AS quoted,
       has_table_privilege('public.priv_t', 'SELECT') AS qualified;
SELECT has_table_privilege('1259', 'SELECT');
-- A schema and a role are stored names, taken verbatim.
SELECT has_schema_privilege('PRIV_SCH', 'USAGE');
SELECT pg_has_role('PRIV_PLAIN', 'USAGE');
-- A type and a function go through regtypein/regprocedurein, which do read
-- all-digits as an OID.
SELECT has_type_privilege('23', 'USAGE') AS by_oid,
       has_type_privilege('integer', 'USAGE') AS by_sql_name,
       has_type_privilege('pg_catalog.int4', 'USAGE') AS qualified;
SELECT has_type_privilege('-', 'USAGE');
SELECT has_function_privilege('nosuch(int4)', 'EXECUTE');
--
-- Misses and malformed arguments.
--
-- An OID that names no relation is NULL, not an error — for a superuser too,
-- because the relation families read the relation's row before anything else.
SELECT has_table_privilege(999999::oid, 'SELECT') AS tab,
       has_sequence_privilege(999999::oid, 'USAGE') AS seq,
       has_any_column_privilege(999999::oid, 'SELECT') AS any_col,
       has_column_privilege(999999::oid, 1::smallint, 'SELECT') AS col;
-- A column number no column carries is NULL; a system column is a column.
SELECT has_column_privilege('priv_t', 1::smallint, 'SELECT') AS first,
       has_column_privilege('priv_t', 2::smallint, 'SELECT') AS second,
       has_column_privilege('priv_t', 0::smallint, 'SELECT') AS zero,
       has_column_privilege('priv_t', 9::smallint, 'SELECT') AS past_end,
       has_column_privilege('priv_t', (-1)::smallint, 'SELECT') AS system;
-- Every one of them is STRICT.
SELECT has_table_privilege(NULL, 'SELECT') AS rel,
       has_table_privilege('priv_t', NULL) AS priv,
       pg_has_role(NULL, 'USAGE') AS role;
-- A name that names nothing is the class's own error.
SELECT has_table_privilege('priv_nosuch', 'SELECT');
SELECT has_schema_privilege('priv_nosuch', 'USAGE');
SELECT has_type_privilege('priv_nosuch', 'USAGE');
SELECT pg_has_role('priv_nosuch', 'USAGE');
SELECT has_table_privilege('priv_nosuch', 'priv_t', 'SELECT');
SELECT has_column_privilege('priv_t', 'nosuch', 'SELECT');
-- A qualified name whose *schema* is missing reports the schema, not the
-- relation: the relation is never looked for.
SELECT has_table_privilege('priv_nosuch.priv_t', 'SELECT');
-- An unrecognized privilege is 22023, and each class recognizes its own set.
SELECT has_table_privilege('priv_t', 'BOGUS');
SELECT has_table_privilege('priv_t', 'USAGE');
SELECT has_schema_privilege('priv_sch', 'SELECT');
SELECT has_type_privilege('int4', 'SELECT');
SELECT has_column_privilege('priv_t', 'a', 'TRUNCATE');
SELECT pg_has_role('priv_plain', 'EXECUTE');
SELECT has_table_privilege('priv_t', '');
SELECT has_table_privilege('priv_t', 'SELECT WITH  GRANT OPTION');
-- The role is resolved before the relation, the relation before the column,
-- and the column before the privilege string.
SELECT has_table_privilege('priv_nosuch', 'priv_nosuch', 'BOGUS');
SELECT has_column_privilege('priv_t', 'nosuch', 'BOGUS');
-- A sequence is the one family that checks the relation kind — and it does so
-- after the privilege string, not before.
SELECT has_sequence_privilege('priv_t', 'BOGUS');
SELECT has_sequence_privilege('priv_t', 'SELECT');
DROP TABLE priv_t;
DROP SEQUENCE priv_s;
DROP SCHEMA priv_sch;
DROP ROLE priv_plain;
DROP ROLE priv_member;
DROP ROLE priv_noinherit;
DROP ROLE priv_indirect;
DROP ROLE priv_indirect_noinherit;
DROP ROLE priv_admin;
DROP ROLE priv_group;
