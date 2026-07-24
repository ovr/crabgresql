--
-- RANGE partitioning: DDL + catalog reflection, tuple routing, and union scans.
-- Declare a RANGE-partitioned parent and attach leaf partitions, reflect them
-- through pg_class (relkind='p' parent, relispartition partitions), pg_inherits,
-- and pg_partitioned_table, then INSERT into and SELECT from the parent: rows
-- route to the leaf whose range admits the key, and a parent scan unions every
-- partition. Every statement's output is hand-checked against PostgreSQL
-- (psql -a -q). UPDATE/DELETE and TRUNCATE/CREATE INDEX on the parent are still
-- unsupported (0A000).
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
-- A leaf enforces its own bound on a direct INSERT. A key below its range is
-- rejected (23514); the DETAIL shows the failing row, and nothing is written.
INSERT INTO sales_2024 VALUES (3, '2023-03-01', 50);
-- FROM is inclusive but TO is exclusive: the upper bound value belongs to the
-- next partition, not this one (23514).
INSERT INTO sales_2024 VALUES (4, '2025-01-01', 50);
-- A NULL partition key has no place in any range partition (23514).
INSERT INTO sales_2024 VALUES (5, NULL, 50);
-- The rejected rows wrote nothing: only the first row remains.
SELECT count(*) FROM sales_2024;
-- Routing an INSERT through the parent sends each row to the leaf whose RANGE
-- bound admits its key (sold): '2023-03-01' lands in sales_2023.
INSERT INTO sales VALUES (2, '2023-03-01', 50);
-- The routed row is physically in sales_2023, not sales_2024.
SELECT * FROM sales_2023;
-- A key admitted by no partition's range is rejected (23514); nothing routes.
INSERT INTO sales VALUES (6, '2020-01-01', 10);
-- A NULL partition key belongs to no range partition, so it too is rejected.
INSERT INTO sales VALUES (7, NULL, 10);
-- Scanning the parent unions every partition; ORDER BY sorts across the union.
SELECT * FROM sales ORDER BY sold;
-- The union scan feeds WHERE and aggregates like an ordinary table.
SELECT count(*) FROM sales;
SELECT id, amount FROM sales WHERE amount >= 100 ORDER BY id;
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
