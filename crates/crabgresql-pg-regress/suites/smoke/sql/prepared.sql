--
-- PREPARED
-- SQL-level PREPARE / EXECUTE / DEALLOCATE: parameter typing and coercion, the
-- command tags each form reports, name folding, and pg_prepared_statements.
--
CREATE TABLE pt (a integer, b text);
-- A declared type list seeds the parameter types; an omitted one leaves them to
-- be deduced from how the statement uses them.
PREPARE p1 (INT, TEXT) AS SELECT $1, $2;
PREPARE p2 AS SELECT 1;
PREPARE p3 AS SELECT a, b FROM pt WHERE a = $1;
EXECUTE p2;
-- Arguments are ordinary expressions, evaluated before the statement runs.
EXECUTE p1 (1 + 1, 'a' || 'b');
-- A name may not be prepared twice while it exists.
PREPARE p1 AS SELECT 2;
-- Wrong argument counts are refused, with the counts in the DETAIL.
EXECUTE p1 (1);
EXECUTE p1;
-- ... but a statement that declared no parameters ignores whatever it is given,
-- without even evaluating it.
EXECUTE p2 (1 / 0);
-- An unknown name is 26000 from either statement.
EXECUTE nosuch (1);
DEALLOCATE nosuch;
-- Only an optimizable statement can be prepared; a utility statement is a
-- syntax error at its leading keyword. The refusal is covered by the e2e tests
-- rather than here, because PostgreSQL also points a caret at that keyword.
-- TODO: move those cases here once the parser records a span for PREPARE.
-- VALUES and a leading WITH are SELECTs, so both prepare.
PREPARE v1 AS VALUES (1), (2);
PREPARE w1 AS WITH c AS (SELECT 1 AS x) SELECT * FROM c;
EXECUTE v1;
EXECUTE w1;
-- Arguments are assignment-coerced to the declared type: numeric rounds into an
-- int, an unknown literal goes through the type's input function, and a type
-- that only an explicit cast could convert is refused.
PREPARE q (INT) AS SELECT $1;
EXECUTE q (1.7);
EXECUTE q (1.4);
EXECUTE q ('7');
EXECUTE q (NULL);
EXECUTE q ('abc');
EXECUTE q (true);
-- EXECUTE reports the tag of the statement it names.
PREPARE ins (INT, TEXT) AS INSERT INTO pt VALUES ($1, $2);
PREPARE upd AS UPDATE pt SET b = 'y';
PREPARE del AS DELETE FROM pt RETURNING *;
EXECUTE ins (1, 'x');
EXECUTE upd;
EXECUTE del;
-- Names fold like every other identifier.
PREPARE Foo AS SELECT 1;
EXECUTE FOO;
PREPARE "Bar" AS SELECT 1;
-- A user-defined type names itself in the regtype arrays, as any other does.
CREATE TYPE mood AS ENUM ('ok', 'bad');
PREPARE m (mood) AS SELECT $1;
SELECT parameter_types, result_types FROM pg_prepared_statements WHERE name = 'm';
-- EXPLAIN EXECUTE plans the statement the name refers to, with its own arguments.
INSERT INTO pt VALUES (1, 'x'), (2, 'y');
PREPARE pa (int) AS SELECT a, b FROM pt WHERE a = $1;
EXPLAIN (COSTS OFF) EXECUTE pa (2);
DELETE FROM pt;
-- A statement with no result set reports NULL result_types, not an empty array.
SELECT name, statement, parameter_types, result_types, from_sql
  FROM pg_prepared_statements WHERE name NOT IN ('m', 'pa') ORDER BY name;
SELECT prepare_time > now() - interval '1 hour' AS recent
  FROM pg_prepared_statements WHERE name = 'p2';
-- Executions are counted per plan kind: a statement with no parameters has
-- nothing to specialize on, so it is planned generically from the first run.
EXECUTE pa (1);
EXECUTE pa (2);
EXECUTE p2;
SELECT name, generic_plans, custom_plans FROM pg_prepared_statements
  WHERE name IN ('pa', 'p2') ORDER BY name;
-- PREPARE is not transactional: a rolled-back block leaves its statement behind.
BEGIN;
PREPARE t1 AS SELECT 1;
ROLLBACK;
EXECUTE t1;
-- DEALLOCATE takes one name, or ALL — which is a keyword only unquoted.
DEALLOCATE "ALL";
DEALLOCATE PREPARE p2;
DEALLOCATE p1;
SELECT count(*) > 0 AS some_left FROM pg_prepared_statements;
DEALLOCATE ALL;
SELECT count(*) FROM pg_prepared_statements;
DROP TYPE mood;
DROP TABLE pt;
