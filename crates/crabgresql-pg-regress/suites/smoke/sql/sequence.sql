--
-- SEQUENCES
-- A sequence is a named counter: nextval advances and returns it, currval and
-- lastval read this session's last value, setval repositions it. The counter is
-- NON-transactional (a ROLLBACK does not undo an advance). `serial` desugars to
-- an integer column plus an owned sequence and a nextval(...) default, and a
-- sequence reflects into the catalogs as relkind 'S' / pg_sequence. Output
-- hand-checked against PostgreSQL (psql -a -q).
--
CREATE SEQUENCE sq_a INCREMENT 2 MINVALUE 10 MAXVALUE 20 START 10;
-- The first nextval returns the start value, then advances by the increment.
SELECT nextval('sq_a');
SELECT nextval('sq_a');
-- currval and lastval report this session's most recent value.
SELECT currval('sq_a');
SELECT lastval();
-- The regclass form of the argument is accepted too.
SELECT nextval('sq_a'::regclass);
-- setval repositions the counter; the next nextval reflects is_called.
SELECT setval('sq_a', 10);
SELECT nextval('sq_a');
SELECT setval('sq_a', 18, false);
SELECT nextval('sq_a');
-- Reaching the maximum (20) with NO CYCLE is an error on the next advance.
SELECT nextval('sq_a');
SELECT nextval('sq_a');
-- currval before nextval in a fresh sequence is an error.
CREATE SEQUENCE sq_b;
SELECT currval('sq_b');
-- The advance is non-transactional: a ROLLBACK does not rewind it.
SELECT nextval('sq_b');
BEGIN;
SELECT nextval('sq_b');
ROLLBACK;
SELECT nextval('sq_b');
-- serial columns auto-assign from an owned sequence.
CREATE TABLE sq_t (id serial PRIMARY KEY, name text);
INSERT INTO sq_t (name) VALUES ('a'), ('b');
INSERT INTO sq_t (name) VALUES ('c');
SELECT id, name FROM sq_t ORDER BY id;
-- A sequence reflects into pg_class as relkind 'S'...
SELECT relname, relkind FROM pg_class WHERE relname IN ('sq_a', 'sq_t_id_seq') ORDER BY relname;
-- ... and its parameters into pg_sequence.
SELECT seqstart, seqincrement, seqmin, seqmax, seqcycle
  FROM pg_sequence JOIN pg_class ON pg_class.oid = pg_sequence.seqrelid
  WHERE relname = 'sq_a';
-- A sequence is not a table, so it is absent from information_schema.tables.
SELECT count(*) FROM information_schema.tables WHERE table_name = 'sq_a';
-- DROP SEQUENCE on a table is a wrong-object-type error.
DROP SEQUENCE sq_t;
-- DROP TABLE auto-drops the owned serial sequence.
DROP TABLE sq_t;
SELECT count(*) FROM pg_class WHERE relname = 'sq_t_id_seq';
-- Missing-object diagnostics, with and without IF EXISTS.
DROP SEQUENCE sq_missing;
DROP SEQUENCE IF EXISTS sq_missing;
-- Clean up (this suite shares one database across tests).
DROP SEQUENCE sq_a;
DROP SEQUENCE sq_b;
-- `nextval`/`currval`/`setval` take a `regclass`, not text, so an unquoted name
-- in the literal is normalized the way the parser would normalize it: folded to
-- lower case, surrounding whitespace ignored. A double-quoted name inside the
-- literal keeps its case.
CREATE SEQUENCE sq_reg;
CREATE SEQUENCE "sqCase";
SELECT nextval('sq_reg'), nextval('SQ_REG'), nextval('  sq_reg  ');
SELECT currval('SQ_REG');
SELECT setval('SQ_REG', 10);
SELECT nextval('"sqCase"');
-- a serial column's default is stored already deparsed, carrying the cast that
-- `nextval`'s argument type needs
CREATE TABLE sq_serial (id serial, b int);
SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true)
  FROM pg_catalog.pg_attrdef d
 WHERE d.adrelid = 'sq_serial'::regclass;
INSERT INTO sq_serial (b) VALUES (1), (2);
SELECT id, b FROM sq_serial ORDER BY id;
DROP TABLE sq_serial;
DROP SEQUENCE "sqCase";
DROP SEQUENCE sq_reg;
