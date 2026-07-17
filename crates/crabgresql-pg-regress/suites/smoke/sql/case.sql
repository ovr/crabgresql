--
-- CASE
-- Searched and simple CASE expressions: branch selection, laziness, result-type
-- unification, and error cases, checked against psql's aligned format.
--
-- searched CASE in the SELECT list; the default column name is "case"
SELECT CASE WHEN true THEN 'yes' WHEN false THEN 'no' END;
-- first true branch wins; later true branches are ignored
SELECT CASE WHEN 1 > 2 THEN 'a' WHEN 2 > 1 THEN 'b' ELSE 'c' END AS pick;
-- a NULL condition behaves like false and falls through to ELSE
SELECT CASE WHEN NULL THEN 'x' ELSE 'y' END AS via_else;
-- no branch matches and no ELSE: result is NULL (renders empty)
SELECT CASE WHEN false THEN 1 END AS none;
-- simple CASE: operand compared for equality against each WHEN value
SELECT CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'many' END AS word;
-- result branches of differing numeric types unify to the common type
SELECT CASE WHEN true THEN 1 ELSE 2.5 END AS promoted;
-- CASE over a table column, once per row
CREATE TABLE grades (name text, score integer);
INSERT INTO grades VALUES ('a', 91), ('b', 72), ('c', 55), ('d', NULL);
SELECT name,
       CASE WHEN score >= 90 THEN 'A'
            WHEN score >= 70 THEN 'B'
            WHEN score IS NULL THEN 'n/a'
            ELSE 'F' END AS grade
  FROM grades;
-- CASE in a WHERE predicate
SELECT name FROM grades
 WHERE CASE WHEN score IS NULL THEN false ELSE score >= 70 END;
-- error: a searched WHEN condition must be boolean
SELECT CASE WHEN 1 THEN 'x' END;
-- error: incompatible concrete result types cannot be matched
SELECT CASE WHEN true THEN 1 ELSE true END;
-- error: an untyped result literal that does not fit the resolved type
SELECT CASE WHEN true THEN 1 ELSE 'x' END;
