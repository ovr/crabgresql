--
-- PG_DATABASE / PG_TABLESPACE / THE ROLE RELATIONS
--
-- Output hand-written rather than copied from a stock PostgreSQL: the database
-- and role names are this server's (the regress client connects as "postgres"
-- to "regression"), the database OID is crabgresql's own, and the locale
-- columns report C because the default collation compares bytewise. The column
-- lists and every other value are PostgreSQL's, probed from 18.4.
--
-- exactly one database: the one this session is connected to
SELECT datname, datdba, encoding, datlocprovider, datistemplate, datallowconn,
       dathasloginevt, datconnlimit, dattablespace, datcollate, datctype
  FROM pg_database;
SELECT datname = current_database() AS is_the_connected_one,
       datlocale IS NULL AND daticurules IS NULL AND datcollversion IS NULL
         AS locale_columns_are_null
  FROM pg_database;
-- the encoding column is a number that resolves through the encoding table
SELECT pg_encoding_to_char(encoding) FROM pg_database;
-- the two bootstrap tablespaces, as in PostgreSQL; nothing creates a third
SELECT oid, spcname, spcowner, spcoptions FROM pg_tablespace ORDER BY oid;
-- one role, the bootstrap superuser, whose OID is PostgreSQL's own 10
SELECT oid, rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb,
       rolcanlogin, rolreplication, rolbypassrls, rolconnlimit
  FROM pg_authid;
SELECT rolname = current_user AS is_the_session_user,
       rolpassword IS NULL AS no_password,
       rolvaliduntil IS NULL AS no_expiry
  FROM pg_authid;
-- pg_roles is pg_authid with the password masked; note PostgreSQL's own column
-- order, which differs from pg_authid's
SELECT rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin,
       rolreplication, rolconnlimit, rolpassword, rolvaliduntil, rolbypassrls,
       rolconfig, oid
  FROM pg_roles;
-- pg_user is the login roles under the pre-8.1 names, also masked...
SELECT usename, usesysid, usecreatedb, usesuper, userepl, usebypassrls, passwd,
       valuntil, useconfig
  FROM pg_user;
-- ...and pg_shadow is the same view with the password column unmasked, which
-- here is NULL because nothing stores one
SELECT usename, usesysid, passwd IS NULL AS no_password FROM pg_shadow;
-- pg_group holds the roles that CANNOT log in. It is empty as a consequence of
-- that rule, not as a constant: crabgresql's one role is a login role, and it
-- creates none of PostgreSQL's predefined pg_read_all_data/pg_monitor/... roles.
SELECT count(*) AS groups FROM pg_group;
-- role membership needs two roles and a GRANT, and there is neither
SELECT count(*) AS members FROM pg_auth_members;
-- every one of these resolves by name through the fixed OID table, so a client
-- that hardcodes a catalog OID means the same relation here
SELECT 'pg_database'::regclass, 'pg_tablespace'::regclass,
       'pg_authid'::regclass, 'pg_auth_members'::regclass;
SELECT 1262::regclass, 1213::regclass, 1260::regclass, 1261::regclass;
SELECT 'pg_roles'::regclass, 'pg_user'::regclass, 'pg_shadow'::regclass,
       'pg_group'::regclass;
