--
-- WITH ORDINALITY
-- A set-returning function in FROM position numbered by a trailing bigint
-- column. Covers both FROM spellings (a call with arguments and the parser's
-- own UNNEST factor), how aliases interact with the added column, referencing
-- it in WHERE/ORDER BY, and the shape psql's `\d` trigger footer uses
-- (pg_partition_ancestors ... WITH ORDINALITY AS a(relid, depth)). Every
-- statement's output is hand-checked against PostgreSQL (psql -a -q).
--
-- The added column is named `ordinality` and numbers the function's rows from 1.
SELECT * FROM generate_series(5, 8) WITH ORDINALITY;
-- Its type is bigint, whatever the function's own column is.
SELECT pg_typeof(ordinality) FROM generate_series(1, 1) WITH ORDINALITY;
-- A bare alias names a scalar function's own column only — the ordinal keeps
-- its name.
SELECT * FROM generate_series(5, 7) WITH ORDINALITY t;
-- An alias column list renames the ordinal like any other column, and a list
-- shorter than the rowset renames the leading columns only.
SELECT * FROM generate_series(5, 7) WITH ORDINALITY AS s(a, b);
SELECT * FROM generate_series(5, 7) WITH ORDINALITY AS s(a);
-- One name past the widened rowset is 42P10.
SELECT * FROM generate_series(5, 7) WITH ORDINALITY AS s(a, b, c);
-- The ordinal is an ordinary column: it projects, filters and sorts.
SELECT ordinality, g FROM generate_series(10, 40, 10) WITH ORDINALITY AS s(g)
  WHERE ordinality > 1 ORDER BY ordinality DESC;
-- An empty rowset stays empty; the numbering starts at 1 for the first row the
-- function actually produced, not the first row that survives a filter.
SELECT * FROM generate_series(1, 0) WITH ORDINALITY;
SELECT * FROM generate_series(1, 5) WITH ORDINALITY AS s(g) WHERE g > 3;
-- The parser gives UNNEST its own FROM factor; it reaches the same rowset.
SELECT * FROM unnest(ARRAY['a', 'b', 'c']) WITH ORDINALITY;
SELECT * FROM unnest(ARRAY[10, 20]) WITH ORDINALITY AS u(elem, pos);
-- NULL elements are numbered like any other row.
SELECT * FROM unnest(ARRAY[1, NULL, 3]) WITH ORDINALITY AS u(elem, pos);
-- A record-returning function keeps its row type's column names and gets the
-- ordinal appended.
SELECT * FROM pg_input_error_info('1e400', 'float4') WITH ORDINALITY;
-- In a join the function is a row source like any other: the ordinal reaches the
-- result and pairs with every outer row.
CREATE TABLE letters (id integer, tag text);
INSERT INTO letters VALUES (1, 'a'), (2, 'b');
SELECT l.tag, s.n, s.ord
  FROM letters l, generate_series(1, 2) WITH ORDINALITY AS s(n, ord)
  ORDER BY l.id, s.ord;
DROP TABLE letters;
-- The trigger-footer shape: pg_partition_ancestors numbered from the partition
-- (depth 1) up to the root.
CREATE TABLE sales (id integer, sold date) PARTITION BY RANGE (sold);
CREATE TABLE sales_2024 PARTITION OF sales FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
SELECT a.relid, a.depth
  FROM pg_catalog.pg_partition_ancestors('sales_2024'::regclass) WITH ORDINALITY AS a(relid, depth)
  ORDER BY a.depth;
-- A relation that is neither a partition nor partitioned yields no rows, so the
-- ordinal never starts.
CREATE TABLE plain (id integer);
SELECT a.relid, a.depth
  FROM pg_catalog.pg_partition_ancestors('plain'::regclass) WITH ORDINALITY AS a(relid, depth)
  ORDER BY a.depth;
DROP TABLE sales;
DROP TABLE plain;
