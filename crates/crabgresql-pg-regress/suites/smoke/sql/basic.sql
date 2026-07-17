--
-- BASIC
-- Smoke test for the regression runner: only M0-supported SQL, expected
-- output hand-checked against psql's aligned format.
--
SELECT 1 AS one;
-- alignment: int right, text/bool left, NULL renders empty (trailing space)
SELECT 1 AS id, 'crab' AS name, true AS ok, NULL AS missing;
CREATE TABLE crabs (id integer, name text, big boolean);
INSERT INTO crabs VALUES (1, 'ferris', true);
INSERT INTO crabs VALUES (2, 'hermit', false), (3, NULL, NULL);
SELECT * FROM crabs;
-- SET is accepted and quiet
SET search_path = public;
-- a statement spanning several lines
SELECT
    42 AS answer,
    'multi line' AS style;
-- two statements on one line: echoed once, two results
SELECT 1; SELECT 2;
