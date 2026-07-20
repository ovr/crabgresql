--
-- DROP VIEW
-- DROP VIEW removes a view, IF EXISTS downgrades a miss to a NOTICE, the wrong
-- object type is reported (a table is "not a view" and a view "not a table"),
-- and a view that other views/tables depend on blocks a RESTRICT drop but
-- cascades under CASCADE. Output hand-checked against PostgreSQL (psql -a -q).
--
CREATE TABLE t (id integer);
INSERT INTO t VALUES (1), (2);
CREATE VIEW v AS SELECT id FROM t;
CREATE VIEW w AS SELECT id FROM v;
-- DROP TABLE on a view (and DROP VIEW on a table) is a wrong-object-type error.
DROP TABLE v;
DROP VIEW t;
-- RESTRICT (the default): the view blocks dropping the table it reads.
DROP TABLE t;
-- ... and w blocks dropping the view v it reads.
DROP VIEW v;
-- DROP VIEW removes a leaf view; afterwards it no longer resolves.
DROP VIEW w;
SELECT id FROM w;
-- A missing view without IF EXISTS errors; with IF EXISTS it is a skip NOTICE.
DROP VIEW w;
DROP VIEW IF EXISTS w;
-- CASCADE drops the table and every view that still depends on it.
DROP TABLE t CASCADE;
SELECT id FROM v;
