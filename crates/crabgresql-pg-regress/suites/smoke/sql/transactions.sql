--
-- TRANSACTIONS
-- Block status and warnings, the aborted-transaction state, isolation levels,
-- access modes, and SET TRANSACTION. Cross-session MVCC rollback/visibility is
-- exercised by the server e2e tests (real undo); this single-session fixture
-- covers the control-flow and transaction-mode surface.
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
-- Isolation levels are accepted on BEGIN and may be changed with SET
-- TRANSACTION before the first query in the block.
BEGIN ISOLATION LEVEL REPEATABLE READ;
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;
SELECT 1 AS one;
COMMIT;
-- SET TRANSACTION after a query in the block is rejected (25001).
BEGIN;
SELECT 1 AS one;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ;
ROLLBACK;
-- SET TRANSACTION outside a block warns but still succeeds.
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;
-- A READ ONLY block allows reads but rejects writes (25006), DML and DDL alike.
BEGIN READ ONLY;
SELECT * FROM t;
INSERT INTO t VALUES (1);
ROLLBACK;
BEGIN READ ONLY;
CREATE TABLE ro_ddl (x int);
ROLLBACK;
-- The session default isolation is settable, and DEFAULT/RESET restore it.
SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ;
SET default_transaction_isolation = 'read committed';
SET default_transaction_isolation = DEFAULT;
RESET default_transaction_isolation;
