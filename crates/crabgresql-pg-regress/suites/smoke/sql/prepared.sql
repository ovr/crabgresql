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
-- syntax error at its leading keyword. The refusal itself is covered by the
-- e2e tests, not here: PostgreSQL points a caret at that keyword and the parser
-- reports no span for PREPARE, so there is nothing to point one at.
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
-- A statement with no result set reports NULL result_types, not an empty array.
SELECT name, statement, parameter_types, result_types, from_sql
  FROM pg_prepared_statements ORDER BY name;
SELECT prepare_time > now() - interval '1 hour' AS recent
  FROM pg_prepared_statements WHERE name = 'p2';
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
DROP TABLE pt;
