--
-- PL/pgSQL
-- The subset this engine runs: declarations, assignment, IF/ELSIF, the three
-- loop forms with labels, EXIT/CONTINUE, RETURN, RAISE, PERFORM, SELECT INTO
-- and embedded DML — plus DO blocks, CREATE PROCEDURE and CALL. Output
-- generated from PostgreSQL (psql -a -q).
--
-- Not covered here: EXCEPTION handlers, SETOF/RETURN QUERY, cursors, EXECUTE
-- and FOR-over-query, all of which report 0A000 by name. Nor the variable/
-- column name conflict, which this engine resolves in the variable's favour
-- where PostgreSQL raises — see the crabgresql-plpgsql crate docs.
--
-- Cases whose only difference from PostgreSQL is a missing `LINE n:` cursor
-- excerpt live in the server e2e tests instead, so this file stays byte-
-- identical to PostgreSQL: a compile error's caret into the body, and the two
-- "is a procedure"/"is not a procedure" errors.
--
CREATE FUNCTION pl_add(a int, b int) RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
  RETURN a + b;
END $$;
SELECT pl_add(2, 3);
-- Arguments are reachable positionally as well as by name.
CREATE FUNCTION pl_dollar(a int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RETURN $1 * 2;
END $$;
SELECT pl_dollar(21);

-- Declarations: initializers, CONSTANT, NOT NULL, and assignment coercing to
-- the declared type.
CREATE FUNCTION pl_decl() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  n int := 7;
  k CONSTANT text := 'x';
  s text;
BEGIN
  s := n;
  RETURN s || k;
END $$;
SELECT pl_decl();

-- IF / ELSIF / ELSE.
CREATE FUNCTION pl_sign(n int) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  IF n > 0 THEN
    RETURN 'positive';
  ELSIF n < 0 THEN
    RETURN 'negative';
  ELSE
    RETURN 'zero';
  END IF;
END $$;
SELECT pl_sign(5), pl_sign(-5), pl_sign(0);

-- FOR over an integer range, forward, REVERSE, and BY a step.
CREATE FUNCTION pl_sum(lo int, hi int) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE t bigint := 0;
BEGIN
  FOR i IN lo..hi LOOP
    t := t + i;
  END LOOP;
  RETURN t;
END $$;
SELECT pl_sum(1, 4);
-- An empty range runs no iterations.
SELECT pl_sum(5, 1);

CREATE FUNCTION pl_steps() RETURNS text LANGUAGE plpgsql AS $$
DECLARE t text := '';
BEGIN
  FOR i IN REVERSE 3..1 LOOP
    t := t || i;
  END LOOP;
  t := t || '/';
  FOR i IN 1..10 BY 3 LOOP
    t := t || i;
  END LOOP;
  RETURN t;
END $$;
SELECT pl_steps();

-- WHILE with EXIT WHEN and CONTINUE WHEN.
CREATE FUNCTION pl_odds() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE i int := 0; t bigint := 0;
BEGIN
  WHILE i < 10 LOOP
    i := i + 1;
    CONTINUE WHEN i % 2 = 0;
    EXIT WHEN i > 7;
    t := t + i;
  END LOOP;
  RETURN t;
END $$;
SELECT pl_odds();

-- A labeled EXIT leaves the named loop, not just the innermost one.
CREATE FUNCTION pl_labeled() RETURNS int LANGUAGE plpgsql AS $$
DECLARE t int := 0;
BEGIN
  <<outer>>
  FOR i IN 1..3 LOOP
    FOR j IN 1..3 LOOP
      t := t + 1;
      EXIT outer WHEN t = 4;
    END LOOP;
  END LOOP;
  RETURN t;
END $$;
SELECT pl_labeled();

-- Nested blocks: an inner declaration shadows an outer one and is restored.
CREATE FUNCTION pl_shadow() RETURNS text LANGUAGE plpgsql AS $$
DECLARE x int := 1; t text := '';
BEGIN
  DECLARE x int := 2;
  BEGIN
    t := t || x;
  END;
  RETURN t || x;
END $$;
SELECT pl_shadow();

-- Recursion.
CREATE FUNCTION pl_fact(n int) RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
  IF n <= 1 THEN
    RETURN 1;
  END IF;
  RETURN n * pl_fact(n - 1);
END $$;
SELECT pl_fact(10);

-- Embedded DML, SELECT INTO, PERFORM and FOUND.
CREATE TABLE pl_t (n int);
CREATE FUNCTION pl_load(v int) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE total bigint;
BEGIN
  INSERT INTO pl_t (n) VALUES (v);
  INSERT INTO pl_t (n) VALUES (v * 2);
  SELECT sum(n) INTO total FROM pl_t;
  RETURN total;
END $$;
SELECT pl_load(5);
SELECT count(*) FROM pl_t;

CREATE FUNCTION pl_found() RETURNS text LANGUAGE plpgsql AS $$
DECLARE x int; t text := '';
BEGIN
  SELECT n INTO x FROM pl_t WHERE n = 999;
  IF NOT FOUND THEN t := t || 'miss'; END IF;
  PERFORM 1;
  IF FOUND THEN t := t || '/hit'; END IF;
  RETURN t;
END $$;
SELECT pl_found();

-- SELECT INTO STRICT: no rows and too many rows are distinct errors.
CREATE FUNCTION pl_strict_none() RETURNS int LANGUAGE plpgsql AS $$
DECLARE x int;
BEGIN
  SELECT n INTO STRICT x FROM pl_t WHERE n = 999;
  RETURN x;
END $$;
SELECT pl_strict_none();
CREATE FUNCTION pl_strict_many() RETURNS int LANGUAGE plpgsql AS $$
DECLARE x int;
BEGIN
  SELECT n INTO STRICT x FROM pl_t;
  RETURN x;
END $$;
SELECT pl_strict_many();

-- RAISE: format expansion, %% and a NULL argument.
CREATE FUNCTION pl_notice(n int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'n is %, 100%% sure, % again', n, NULL;
  RETURN n;
END $$;
SELECT pl_notice(7);

-- RAISE EXCEPTION, with and without an explicit SQLSTATE.
CREATE FUNCTION pl_boom(n int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'bad value %', n USING ERRCODE = '22023', HINT = 'try 1';
END $$;
SELECT pl_boom(7);
CREATE FUNCTION pl_plain() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'plain';
END $$;
SELECT pl_plain();
-- A condition name supplies both the SQLSTATE and the message.
CREATE FUNCTION pl_condition() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RAISE division_by_zero;
END $$;
SELECT pl_condition();

-- Too few and too many RAISE arguments.
CREATE FUNCTION pl_too_few() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'a % b %', 1;
  RETURN 1;
END $$;

-- Falling off the end of a function without RETURN.
CREATE FUNCTION pl_no_return() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  NULL;
END $$;
SELECT pl_no_return();

-- A bare SELECT has nowhere to put its rows.
CREATE FUNCTION pl_no_dest() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  SELECT 1;
  RETURN 1;
END $$;
SELECT pl_no_dest();

-- A body is only syntax-checked at CREATE time, so a forward reference is fine.
CREATE FUNCTION pl_forward() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE c bigint;
BEGIN
  SELECT count(*) INTO c FROM pl_later;
  RETURN c;
END $$;
CREATE TABLE pl_later (n int);
SELECT pl_forward();

-- DO blocks.
DO $$ BEGIN RAISE NOTICE 'from a DO block'; END $$;
DO $$
BEGIN
  FOR i IN 1..3 LOOP
    INSERT INTO pl_later (n) VALUES (i);
  END LOOP;
END $$;
SELECT count(*) FROM pl_later;
DO LANGUAGE plpgsql $$ BEGIN NULL; END $$;

-- Procedures.
CREATE PROCEDURE pl_add_row(v int) LANGUAGE plpgsql AS $$
BEGIN
  INSERT INTO pl_later (n) VALUES (v);
END $$;
CALL pl_add_row(40);
SELECT count(*) FROM pl_later WHERE n = 40;
-- DROP will not cross the two kinds.
DROP FUNCTION pl_add_row(int);
DROP PROCEDURE pl_add_row(int);

-- A NOTICE raised before a failure belongs to the statement that raised it,
-- and must not resurface attached to a later one.
DO $$ BEGIN RAISE NOTICE 'before the failure'; RAISE EXCEPTION 'boom'; END $$;
SELECT 1 AS after_the_failure;

-- One CONTEXT frame per invocation, naming the innermost statement -- not one
-- frame per enclosing IF/LOOP.
CREATE FUNCTION pl_nested() RETURNS int LANGUAGE plpgsql AS $$
BEGIN
  FOR i IN 1..3 LOOP
    IF i = 2 THEN
      RAISE EXCEPTION 'nested boom';
    END IF;
  END LOOP;
  RETURN 0;
END $$;
SELECT pl_nested();
DROP FUNCTION pl_nested();

-- With no message at all, the SQLSTATE is the message.
DO $$ BEGIN RAISE EXCEPTION USING DETAIL = 'more'; END $$;

-- `=` is a synonym for `:=` in a declaration.
DO $$ DECLARE x int = 5; BEGIN RAISE NOTICE 'x=%', x; END $$;

-- A RAISE placeholder/argument mismatch is a syntax error, at definition time.
DO $$ BEGIN RAISE NOTICE 'a % b %', 1; END $$;

-- A READ ONLY transaction rejects DML whatever its expressions call.
CREATE FUNCTION pl_pure(n int) RETURNS int LANGUAGE plpgsql AS $$
BEGIN RETURN n; END $$;
BEGIN READ ONLY;
INSERT INTO pl_later (n) VALUES (pl_pure(1));
ROLLBACK;
SELECT pl_pure(7);
DROP FUNCTION pl_pure(int);
