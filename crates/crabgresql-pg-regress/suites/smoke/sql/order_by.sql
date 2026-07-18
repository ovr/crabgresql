--
-- ORDER BY
-- Sorting by column ordinal, output name/alias, and arbitrary expressions
-- (including columns not in the select list), with ASC/DESC and NULLS
-- placement. Expressions not already projected become hidden sort columns
-- that never reach the client.
--
CREATE TABLE ob (id integer, a integer, b integer, name text);
INSERT INTO ob VALUES (1, 3, 10, 'ferris'), (2, 1, 40, 'hermit'), (3, 2, 20, NULL);
-- ordinal: sort by the second output column (a) ascending
SELECT id, a FROM ob ORDER BY 2;
-- output column name: sort by a
SELECT id, a FROM ob ORDER BY a;
-- descending, with the PG default NULLS FIRST for DESC
SELECT id, name FROM ob ORDER BY name DESC;
-- NULLS LAST override on a descending sort
SELECT id, name FROM ob ORDER BY name DESC NULLS LAST;
-- expression over a column that is not selected: a hidden sort column
SELECT id FROM ob ORDER BY a + b;
-- sort by an output alias
SELECT id, a + b AS total FROM ob ORDER BY total DESC;
-- function in the sort key
SELECT id, name FROM ob ORDER BY upper(name);
-- two keys: by b/10 then by id
SELECT id, a, b FROM ob ORDER BY b DESC, id;
