--
-- RETURNING
-- Smoke test for INSERT/UPDATE/DELETE ... RETURNING. Simple cases only:
-- column refs, *, table.*, computed expressions, aliases, and reordering.
--
CREATE TABLE foo (id integer, name text);
-- INSERT ... RETURNING streams the inserted rows.
INSERT INTO foo VALUES (1, 'one'), (2, 'two') RETURNING id, name;
-- * expands to every column; a computed column can follow with an alias.
INSERT INTO foo VALUES (3, 'three') RETURNING *, id + 10 AS bumped;
-- UPDATE ... RETURNING returns the NEW (post-update) rows via table.*.
UPDATE foo SET name = 'renamed', id = id + 100 WHERE id = 1 RETURNING foo.*;
-- DELETE ... RETURNING returns the deleted (OLD) rows, columns reordered.
DELETE FROM foo WHERE id = 2 RETURNING name, id;
-- Final state.
SELECT * FROM foo ORDER BY id;
