--
-- RANGE partitioning (initial slice: DDL + catalog reflection)
-- Declare a RANGE-partitioned parent and attach leaf partitions, then reflect
-- them through pg_class (relkind='p' parent, relispartition partitions),
-- pg_inherits, and pg_partitioned_table. A row goes into a partition directly.
--
-- crabgresql does not route rows or scan partitions through the parent yet, so
-- INSERT/SELECT on the parent are rejected with SQLSTATE 0A000 — a deliberate,
-- honest limitation that diverges from PostgreSQL (which would succeed). Every
-- other statement's output is hand-checked against PostgreSQL (psql -a -q).
--
CREATE TABLE sales (id integer, sold date, amount integer) PARTITION BY RANGE (sold);
CREATE TABLE sales_2023 PARTITION OF sales FOR VALUES FROM ('2023-01-01') TO ('2024-01-01');
CREATE TABLE sales_2024 PARTITION OF sales FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
-- The parent is relkind='p' (partitioned) with relispartition='f'; each leaf is
-- an ordinary heap (relkind='r') with relispartition='t'.
SELECT relname, relkind, relispartition FROM pg_class
  WHERE relname LIKE 'sales%' ORDER BY relname;
-- The partition key: RANGE ('r') over one column, the 2nd attribute (sold).
SELECT partstrat, partnatts, partattrs FROM pg_partitioned_table;
-- pg_inherits links each partition to its parent.
SELECT c.relname AS partition, p.relname AS parent
  FROM pg_inherits i
  JOIN pg_class c ON c.oid = i.inhrelid
  JOIN pg_class p ON p.oid = i.inhparent
  ORDER BY 1;
-- information_schema reports a partitioned parent as a BASE TABLE.
SELECT table_name, table_type FROM information_schema.tables
  WHERE table_name LIKE 'sales%' ORDER BY table_name;
-- A partition is an ordinary table: rows insert into and select from it directly.
INSERT INTO sales_2024 VALUES (1, '2024-06-01', 100);
SELECT * FROM sales_2024;
-- Routing an INSERT through the parent is not supported yet (0A000).
INSERT INTO sales VALUES (2, '2023-03-01', 50);
-- Scanning the parent (union over partitions) is not supported yet (0A000).
SELECT * FROM sales;
-- An unbounded (MINVALUE/MAXVALUE) RANGE bound is accepted.
CREATE TABLE sales_early PARTITION OF sales FOR VALUES FROM (MINVALUE) TO ('2023-01-01');
-- Overlapping a sibling's range is rejected (42P17).
CREATE TABLE sales_dup PARTITION OF sales FOR VALUES FROM ('2024-06-01') TO ('2024-07-01');
-- An empty range (lower >= upper) is rejected (42P17).
CREATE TABLE sales_empty PARTITION OF sales FOR VALUES FROM ('2025-01-01') TO ('2024-01-01');
-- A NULL bound has no place in the range order; rejected (42P17).
CREATE TABLE sales_null PARTITION OF sales FOR VALUES FROM (NULL) TO ('2027-01-01');
-- A duplicate partition name is a name collision (42P07), not a self-overlap.
CREATE TABLE sales_2024 PARTITION OF sales FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
-- IF NOT EXISTS on an existing partition is a no-op.
CREATE TABLE IF NOT EXISTS sales_2024 PARTITION OF sales FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
-- A partitioned parent has no storage of its own: TRUNCATE and CREATE INDEX on
-- it are not supported yet (0A000); they are not silently no-ops.
TRUNCATE sales;
CREATE INDEX sales_idx ON sales (sold);
-- LIST/HASH strategies are not supported yet (0A000).
CREATE TABLE by_list (id integer) PARTITION BY LIST (id);
-- A non-orderable RANGE key type (json) is rejected at parent create (42704).
CREATE TABLE by_json (j json) PARTITION BY RANGE (j);
-- PARTITION OF a table that is not partitioned is rejected (42809).
CREATE TABLE plain (id integer);
CREATE TABLE plain_part PARTITION OF plain FOR VALUES FROM (1) TO (2);
-- DROP on a partitioned parent cascades to its partitions (no CASCADE needed).
DROP TABLE sales;
SELECT relname FROM pg_class WHERE relname LIKE 'sales%' ORDER BY relname;
DROP TABLE plain;
