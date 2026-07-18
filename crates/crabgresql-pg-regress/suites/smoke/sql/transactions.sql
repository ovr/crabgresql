--
-- TRANSACTIONS
-- Control-flow only: block status, warnings, and the aborted-transaction
-- state. Data rollback is not yet implemented (that is the M2 MVCC engine), so
-- this fixture never relies on ROLLBACK undoing writes.
--
-- COMMIT / ROLLBACK with no open block warn but still succeed.
COMMIT;
ROLLBACK;
-- A redundant BEGIN warns but stays in the same block.
BEGIN;
BEGIN;
COMMIT;
-- An error inside a block aborts it: every later statement but COMMIT/ROLLBACK
-- is rejected until the block ends.
BEGIN;
SELECT * FROM does_not_exist;
SELECT 1;
COMMIT;
-- The session is usable again afterwards.
SELECT 'ok' AS status;
-- TRUNCATE empties a populated table.
CREATE TABLE t (id integer);
INSERT INTO t VALUES (1), (2), (3);
TRUNCATE TABLE t;
SELECT * FROM t;
