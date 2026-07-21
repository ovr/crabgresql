--
-- CREATE SCHEMA
-- CREATE SCHEMA registers a namespace visible in pg_namespace and
-- information_schema.schemata. A duplicate without IF NOT EXISTS errors (42P06);
-- with IF NOT EXISTS it is a skip NOTICE. A pg_-prefixed name is reserved
-- (42939). Output hand-checked against PostgreSQL (psql -a -q).
--
CREATE SCHEMA app;
-- It shows up as a namespace, owned like any user object.
SELECT nspname FROM pg_namespace WHERE nspname = 'app';
SELECT schema_name FROM information_schema.schemata WHERE schema_name = 'app';
-- A duplicate errors; IF NOT EXISTS downgrades it to a skip NOTICE.
CREATE SCHEMA app;
CREATE SCHEMA IF NOT EXISTS app;
-- The pg_ prefix is reserved for system schemas.
CREATE SCHEMA pg_stuff;
DROP SCHEMA app;
