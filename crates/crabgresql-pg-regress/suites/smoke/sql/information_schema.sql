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
-- The seven `_pg_*` type-shape helpers the `columns` and `domains` views are
-- defined over. Called directly with the (typid, atttypmod) pairs PostgreSQL
-- stores, so no catalog lookup is involved.
SELECT information_schema._pg_char_max_length(1043, 14) AS varchar10,
       information_schema._pg_char_max_length(1042, 9) AS char5,
       information_schema._pg_char_max_length(1043, -1) AS varchar_any,
       information_schema._pg_char_max_length(1560, 3) AS bit3,
       information_schema._pg_char_max_length(1562, 8) AS varbit8,
       information_schema._pg_char_max_length(25, -1) AS text,
       information_schema._pg_char_max_length(23, -1) AS int4;
SELECT information_schema._pg_char_octet_length(1043, 14) AS varchar10,
       information_schema._pg_char_octet_length(1043, -1) AS varchar_any,
       information_schema._pg_char_octet_length(25, -1) AS text,
       information_schema._pg_char_octet_length(1560, 3) AS bit3,
       information_schema._pg_char_octet_length(23, -1) AS int4;
SELECT information_schema._pg_numeric_precision(21, -1) AS int2,
       information_schema._pg_numeric_precision(23, -1) AS int4,
       information_schema._pg_numeric_precision(20, -1) AS int8,
       information_schema._pg_numeric_precision(700, -1) AS float4,
       information_schema._pg_numeric_precision(701, -1) AS float8,
       information_schema._pg_numeric_precision(1700, 327686) AS numeric_5_2,
       information_schema._pg_numeric_precision(1700, -1) AS numeric_any,
       information_schema._pg_numeric_precision(25, -1) AS text;
SELECT information_schema._pg_numeric_precision_radix(23, -1) AS int4,
       information_schema._pg_numeric_precision_radix(701, -1) AS float8,
       information_schema._pg_numeric_precision_radix(1700, 327686) AS numeric_5_2,
       information_schema._pg_numeric_precision_radix(1700, -1) AS numeric_any,
       information_schema._pg_numeric_precision_radix(25, -1) AS text;
-- `numeric(4,-2)` stores 264194, and the scale half is read unsigned: 2046.
SELECT information_schema._pg_numeric_scale(23, -1) AS int4,
       information_schema._pg_numeric_scale(701, -1) AS float8,
       information_schema._pg_numeric_scale(1700, 327686) AS numeric_5_2,
       information_schema._pg_numeric_scale(1700, 264194) AS numeric_4_neg2,
       information_schema._pg_numeric_scale(1700, -1) AS numeric_any,
       information_schema._pg_numeric_scale(25, -1) AS text;
SELECT information_schema._pg_datetime_precision(1082, -1) AS date,
       information_schema._pg_datetime_precision(1083, -1) AS time_any,
       information_schema._pg_datetime_precision(1083, 3) AS time3,
       information_schema._pg_datetime_precision(1184, -1) AS timestamptz_any,
       information_schema._pg_datetime_precision(1186, -1) AS interval_any,
       information_schema._pg_datetime_precision(1186, 470286340) AS day_to_second4,
       information_schema._pg_datetime_precision(23, -1) AS int4;
SELECT information_schema._pg_interval_type(1186, -1) AS interval_any,
       information_schema._pg_interval_type(1186, 327679) AS year,
       information_schema._pg_interval_type(1186, 458751) AS year_to_month,
       information_schema._pg_interval_type(1186, 470351871) AS day_to_second,
       information_schema._pg_interval_type(1186, 470286340) AS day_to_second4,
       information_schema._pg_interval_type(1186, 268435459) AS second3,
       information_schema._pg_interval_type(1114, 3) AS timestamp3;
-- NULL in either argument is NULL out; an OID no built-in answers to is NULL.
SELECT information_schema._pg_char_max_length(NULL, 14) AS null_typid,
       information_schema._pg_char_max_length(1043, NULL) AS null_typmod,
       information_schema._pg_numeric_precision(999999, -1) AS unknown_typid,
       information_schema._pg_interval_type(NULL, -1) AS null_interval;
-- The same answers reached through the views the helpers define.
CREATE TABLE is_shape (
  a numeric(5,2),
  b varchar(10),
  c char(5),
  d text,
  e interval day to second(4),
  f bit(3),
  g timestamp(3),
  h int4,
  i float8,
  j date
);
SELECT column_name, character_maximum_length, character_octet_length,
       numeric_precision, numeric_precision_radix, numeric_scale,
       datetime_precision, interval_type
  FROM information_schema.columns
 WHERE table_name = 'is_shape'
 ORDER BY ordinal_position;
DROP TABLE is_shape;
-- `domains` answers the same seven questions off the domain's own modifier.
CREATE DOMAIN is_money AS numeric(9,3);
CREATE DOMAIN is_code AS varchar(8);
CREATE DOMAIN is_span AS interval hour to minute;
SELECT domain_name, character_maximum_length, character_octet_length,
       numeric_precision, numeric_precision_radix, numeric_scale,
       datetime_precision, interval_type
  FROM information_schema.domains
 WHERE domain_schema = 'public' AND domain_name LIKE 'is\_%'
 ORDER BY domain_name;
DROP DOMAIN is_money;
DROP DOMAIN is_code;
DROP DOMAIN is_span;
