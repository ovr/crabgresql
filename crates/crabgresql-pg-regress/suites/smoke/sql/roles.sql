--
-- ROLES
-- CREATE/ALTER/DROP ROLE, role membership, and the six relations over
-- pg_authid: pg_roles, pg_user, pg_shadow, pg_group, pg_auth_members.
--
-- Nothing here prints the login role's name or any OID: the expected output is
-- generated against a PostgreSQL whose bootstrap superuser is whatever its
-- initdb chose, while the regress client connects to crabgresql as "postgres".
-- Everything the suite asserts is therefore either about the roles it creates
-- itself or an equality that holds under both names.
--
-- CREATE ROLE: the defaults are "off" for every attribute except INHERIT.
CREATE ROLE roles_alice LOGIN PASSWORD 'secret';
CREATE ROLE roles_devs;
SELECT rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin,
       rolreplication, rolbypassrls, rolconnlimit, rolvaliduntil
  FROM pg_authid WHERE rolname LIKE 'roles_%' ORDER BY rolname;
-- CREATE USER differs from CREATE ROLE in exactly one attribute; CREATE GROUP
-- in none.
CREATE USER roles_carol;
CREATE GROUP roles_admins;
SELECT rolname, rolcanlogin FROM pg_authid
 WHERE rolname IN ('roles_carol', 'roles_admins') ORDER BY rolname;
-- A password is stored as a SCRAM verifier and shown only by pg_authid and
-- pg_shadow; pg_roles and pg_user print the same mask whether or not one is set.
SELECT rolpassword LIKE 'SCRAM-SHA-256$4096:%' AS verifier
  FROM pg_authid WHERE rolname = 'roles_alice';
SELECT rolname, rolpassword FROM pg_roles
 WHERE rolname LIKE 'roles_%' ORDER BY rolname;
SELECT usename, passwd IS NULL AS no_password FROM pg_shadow
 WHERE usename LIKE 'roles_%' ORDER BY usename;
-- pg_user is the login roles, pg_group the rest.
SELECT usename, usesuper, usecreatedb FROM pg_user
 WHERE usename LIKE 'roles_%' ORDER BY usename;
SELECT groname FROM pg_group WHERE groname LIKE 'roles_%' ORDER BY groname;
-- Membership. (The NOTICE a repeat grant raises names the granting role, so it
-- lives in the e2e tests rather than here.)
GRANT roles_devs TO roles_alice;
SELECT r.rolname AS role, m.rolname AS member, a.admin_option, a.inherit_option,
       a.set_option, a.grantor = (SELECT oid FROM pg_authid WHERE rolname = session_user)
         AS granted_by_me
  FROM pg_auth_members a
  JOIN pg_authid r ON r.oid = a.roleid
  JOIN pg_authid m ON m.oid = a.member
 WHERE r.rolname LIKE 'roles_%' ORDER BY role, member;
-- ...and the group lists its members.
SELECT groname, grolist = ARRAY[(SELECT oid FROM pg_authid WHERE rolname = 'roles_alice')]
         AS lists_alice
  FROM pg_group WHERE groname = 'roles_devs';
-- WITH ADMIN OPTION sets the option; ADMIN OPTION FOR revokes it without
-- dropping the membership.
GRANT roles_devs TO roles_carol WITH ADMIN OPTION;
SELECT m.rolname, a.admin_option FROM pg_auth_members a
  JOIN pg_authid m ON m.oid = a.member
 WHERE m.rolname = 'roles_carol';
REVOKE ADMIN OPTION FOR roles_devs FROM roles_carol;
SELECT m.rolname, a.admin_option FROM pg_auth_members a
  JOIN pg_authid m ON m.oid = a.member
 WHERE m.rolname = 'roles_carol';
REVOKE roles_devs FROM roles_carol;
SELECT count(*) AS memberships FROM pg_auth_members a
  JOIN pg_authid m ON m.oid = a.member WHERE m.rolname = 'roles_carol';
-- A grant that would close a cycle in the role graph is refused.
GRANT roles_alice TO roles_devs;
-- GRANTED BY records another role as the grantor. The role must exist, the
-- caller must be able to act as it, and it must hold ADMIN OPTION on what it
-- hands out.
GRANT roles_devs TO roles_alice GRANTED BY roles_nope;
GRANT roles_devs TO roles_alice GRANTED BY roles_alice;
GRANT roles_devs TO roles_admins WITH ADMIN OPTION;
GRANT roles_devs TO roles_carol GRANTED BY roles_admins;
SELECT g.rolname AS grantor FROM pg_auth_members a
  JOIN pg_authid g ON g.oid = a.grantor
  JOIN pg_authid m ON m.oid = a.member
 WHERE m.rolname = 'roles_carol';
-- A revoke reaches only the grant the revoking role made; naming the grantor
-- reaches that one.
REVOKE roles_devs FROM roles_carol GRANTED BY roles_admins;
SELECT count(*) AS memberships FROM pg_auth_members a
  JOIN pg_authid m ON m.oid = a.member WHERE m.rolname = 'roles_carol';
-- ALTER ROLE: attributes, and the SET/RESET pair behind rolconfig.
ALTER ROLE roles_alice WITH CREATEDB CONNECTION LIMIT 3 VALID UNTIL '2030-01-01 00:00:00+00';
-- ...read in UTC, since the two servers' session zones differ.
SELECT rolcreatedb, rolconnlimit, rolvaliduntil AT TIME ZONE 'UTC' AS valid_until
  FROM pg_authid WHERE rolname = 'roles_alice';
ALTER ROLE roles_alice SET extra_float_digits = -3;
SELECT rolconfig FROM pg_roles WHERE rolname = 'roles_alice';
ALTER ROLE roles_alice SET extra_float_digits = 3;
ALTER ROLE roles_alice SET timezone = 'UTC';
SELECT rolconfig FROM pg_roles WHERE rolname = 'roles_alice';
ALTER ROLE roles_alice RESET timezone;
SELECT rolconfig FROM pg_roles WHERE rolname = 'roles_alice';
-- `VALID UNTIL 'infinity'` is a value like any other: PostgreSQL stores and
-- shows `infinity` rather than putting the column back to NULL.
ALTER ROLE roles_alice VALID UNTIL 'infinity';
SELECT rolvaliduntil FROM pg_authid WHERE rolname = 'roles_alice';
ALTER ROLE roles_alice RESET ALL;
SELECT rolconfig IS NULL AS reset FROM pg_roles WHERE rolname = 'roles_alice';
-- RENAME TO, and the duplicate it can collide with.
ALTER ROLE roles_carol RENAME TO roles_bob;
SELECT rolname FROM pg_authid WHERE rolname LIKE 'roles_%' ORDER BY rolname;
ALTER ROLE roles_bob RENAME TO roles_devs;
-- SET ROLE moves current_user and leaves session_user; RESET puts it back.
SET ROLE roles_devs;
SELECT current_user, current_user = session_user AS same;
SHOW role;
RESET ROLE;
SELECT current_user = session_user AS same;
SHOW role;
-- `DEFAULT` is not a role name: PostgreSQL's grammar reserves the word, and
-- `SET role = DEFAULT` is the spelling that resets.
SET ROLE DEFAULT;
-- SET LOCAL outside a block warns and changes nothing.
SET LOCAL ROLE roles_devs;
SELECT current_user = session_user AS same;
SET LOCAL SESSION AUTHORIZATION roles_alice;
SELECT current_user = session_user AS same;
-- SET LOCAL is undone with its transaction.
BEGIN;
SET LOCAL ROLE roles_devs;
SELECT current_user;
ROLLBACK;
SELECT current_user = session_user AS same;
-- RESET ALL leaves the identity parameters alone (GUC_NO_RESET_ALL).
SET ROLE roles_devs;
RESET ALL;
SELECT current_user;
RESET ROLE;
-- SET SESSION AUTHORIZATION moves both, and the role it installs cannot reach
-- a third role it is not a member of.
SET SESSION AUTHORIZATION roles_alice;
SELECT current_user, session_user;
SET ROLE roles_devs;
SELECT current_user, session_user;
RESET ROLE;
SET ROLE roles_bob;
RESET SESSION AUTHORIZATION;
SELECT current_user = session_user AS same;
-- The errors, each under the SQLSTATE PostgreSQL raises it with.
CREATE ROLE roles_alice;
DROP ROLE roles_nope;
DROP ROLE IF EXISTS roles_nope;
ALTER ROLE roles_nope WITH LOGIN;
SET ROLE roles_nope;
-- Dropping a role takes its memberships with it.
DROP ROLE roles_devs;
SELECT count(*) AS memberships FROM pg_auth_members a
  JOIN pg_authid m ON m.oid = a.member WHERE m.rolname LIKE 'roles_%';
DROP ROLE roles_alice;
DROP ROLE roles_bob;
DROP GROUP roles_admins;
SELECT count(*) AS left_over FROM pg_authid WHERE rolname LIKE 'roles_%';
