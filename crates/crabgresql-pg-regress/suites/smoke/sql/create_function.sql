--
-- CREATE FUNCTION ... LANGUAGE SQL
-- A LANGUAGE SQL function with a scalar, FROM-less body is expanded inline at
-- each call: the arguments are referenced by `$1..$n` or by their declared
-- names, the body may call other functions, and the result is coerced to the
-- declared return type. Overloading is by argument type. Output hand-checked
-- against PostgreSQL (psql -a -q).
-- (Error paths — return-type mismatch, duplicate signature — are covered by the
-- server e2e tests, where PG emits a CONTEXT line this engine does not.)
--
CREATE FUNCTION add(int, int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 + $2 $$;
SELECT add(1, 2);
-- The RETURN <expr> body form is equivalent.
CREATE FUNCTION inc(int) RETURNS int LANGUAGE SQL RETURN $1 + 1;
SELECT inc(41);
-- Arguments are arbitrary expressions, and a body may call another function.
CREATE FUNCTION double_inc(int) RETURNS int LANGUAGE SQL AS $$ SELECT inc(inc($1)) $$;
SELECT double_inc(40), add(inc(1), 5);
-- An int body widens to the declared bigint return type.
CREATE FUNCTION widen(int) RETURNS bigint LANGUAGE SQL AS $$ SELECT $1 $$;
SELECT widen(5);
-- Overloading by argument type: same name, different signature.
CREATE FUNCTION same(int) RETURNS int LANGUAGE SQL AS $$ SELECT $1 * 10 $$;
CREATE FUNCTION same(text) RETURNS text LANGUAGE SQL AS $$ SELECT $1 || '!' $$;
SELECT same(4), same('hi');
-- An argument declared with a name is reachable under that name as well as as
-- `$n`, bare or qualified by the routine's own name. A quoted name keeps case.
CREATE FUNCTION named(value int4, seed int8) RETURNS int8 LANGUAGE SQL AS $$ SELECT value + seed $$;
CREATE FUNCTION qualified(value int4, seed int8) RETURNS int8 LANGUAGE SQL AS $$ SELECT qualified.value + qualified.seed $$;
CREATE FUNCTION mixed(value int4, seed int8) RETURNS int8 LANGUAGE SQL AS $$ SELECT value + $2 $$;
CREATE FUNCTION quoted("Value" int4) RETURNS int4 LANGUAGE SQL AS $$ SELECT "Value" * 2 $$;
SELECT named(2, 40), qualified(2, 40), mixed(2, 40), quoted(21);
-- A function applied per row over a table.
CREATE TABLE t (a int, b int);
INSERT INTO t VALUES (1, 2), (3, 4), (10, 20);
SELECT a, b, add(a, b) FROM t ORDER BY a;
-- The SQL-standard body forms, and how pg_get_function_sqlbody renders them
-- back: a RETURN expression, and a single-statement BEGIN ATOMIC block. Such a
-- body leaves prosrc empty — the body lives in prosqlbody alone — while a
-- quoted body keeps prosrc and has no standard body at all.
CREATE FUNCTION body_ret(a int) RETURNS int LANGUAGE SQL RETURN a + 1;
CREATE FUNCTION body_atomic(a int, b int) RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT a + b; END;
CREATE FUNCTION body_atomic_col(a int) RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT a; END;
CREATE FUNCTION body_quoted(a int) RETURNS int LANGUAGE SQL AS $$ SELECT a + 1 $$;
-- A subscripted argument is parenthesised, which a subscripted column in a view
-- is not: the rule wraps the container of a subscript unless it is a plain
-- column, and an argument is not one.
CREATE FUNCTION body_sub(a int[]) RETURNS int LANGUAGE SQL RETURN a[1];
CREATE FUNCTION body_atomic_sub(a int[]) RETURNS int LANGUAGE SQL BEGIN ATOMIC SELECT a[1] + 1; END;
-- A subscript's index is deparsed like any other expression, so the operator in
-- it is wrapped; a cast parenthesises its operand, and the alias follows the
-- operand's own name rather than the target type -- the type names only a
-- target that names nothing.
CREATE FUNCTION body_index(a int[], i int) RETURNS int LANGUAGE SQL RETURN a[i + 1];
CREATE FUNCTION body_cast(a int) RETURNS text LANGUAGE SQL BEGIN ATOMIC SELECT a::text; END;
CREATE FUNCTION body_cast_expr(a int) RETURNS text LANGUAGE SQL BEGIN ATOMIC SELECT (a + 1)::text; END;
CREATE FUNCTION body_array(a int[]) RETURNS int[] LANGUAGE SQL BEGIN ATOMIC SELECT ARRAY[a[1], 2]; END;
SELECT body_ret(1), body_atomic(1, 2), body_atomic_col(41), body_quoted(1);
SELECT body_sub(ARRAY[7, 8]), body_atomic_sub(ARRAY[7, 8]);
SELECT body_index(ARRAY[7, 8], 1), body_cast(3), body_cast_expr(3), body_array(ARRAY[7, 8]);
SELECT proname, prosrc, pg_get_function_sqlbody(oid) AS sqlbody
  FROM pg_proc WHERE proname LIKE 'body\_%' ORDER BY proname;
-- Clean up (this suite shares one database across tests).
DROP TABLE t;
