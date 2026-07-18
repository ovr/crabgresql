--
-- DROP TABLE
-- Create a table, drop it, and confirm it is gone; then exercise the
-- missing-table paths: a bare DROP errors (SQLSTATE 42P01, noun "table"),
-- DROP ... IF EXISTS warns and succeeds, and the name is free to reuse.
-- Output hand-checked against PostgreSQL (psql -a -q).
--
CREATE TABLE dt (a integer, b text);
INSERT INTO dt VALUES (1, 'one'), (2, 'two');
SELECT * FROM dt ORDER BY a;
-- Dropping the table removes it: the following SELECT no longer resolves it.
DROP TABLE dt;
SELECT * FROM dt;
-- A bare DROP of a missing table is an error; DROP TABLE names the object a
-- "table" (not "relation").
DROP TABLE dt;
-- IF EXISTS downgrades the missing table to a skip NOTICE and still succeeds.
DROP TABLE IF EXISTS dt;
-- CASCADE is accepted on a table with no dependents (behaves like RESTRICT).
CREATE TABLE dt (a integer);
DROP TABLE dt CASCADE;
-- The name is reusable after a drop.
CREATE TABLE dt (a integer);
INSERT INTO dt VALUES (42);
SELECT * FROM dt;
DROP TABLE dt;
