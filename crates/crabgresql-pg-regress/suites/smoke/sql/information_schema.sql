-- INFORMATION_SCHEMA
-- Core discovery views are schema-qualified, live, and derived from the table
-- metadata CrabgreSQL currently represents.
CREATE TABLE is_demo (id int4, label varchar(12), created_at timestamptz);
SELECT table_catalog, table_schema, table_name, table_type, is_insertable_into, is_typed
  FROM information_schema.tables
 WHERE table_name = 'is_demo';
SELECT column_name, ordinal_position, data_type, character_maximum_length,
       datetime_precision, udt_schema, udt_name, is_generated, is_updatable
  FROM information_schema.columns
 WHERE table_name = 'is_demo'
 ORDER BY ordinal_position;
SELECT schema_name, schema_owner
  FROM information_schema.schemata
 WHERE schema_name IN ('information_schema', 'pg_catalog', 'public')
 ORDER BY schema_name;
CREATE TEMP TABLE is_tmp (v int4);
SELECT table_name, table_type
  FROM information_schema.tables
 WHERE table_name = 'is_tmp';
DROP TABLE is_demo;
SELECT table_name FROM information_schema.tables WHERE table_name = 'is_demo';
SELECT * FROM tables;
INSERT INTO information_schema.tables VALUES (1);
