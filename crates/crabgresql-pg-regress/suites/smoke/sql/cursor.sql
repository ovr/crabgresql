--
-- CURSOR
-- DECLARE / FETCH / MOVE / CLOSE: every FETCH direction, the transaction
-- scoping rules (block-scoped vs WITH HOLD), NO SCROLL, and pg_cursors.
--
CREATE TABLE ten (g integer);
INSERT INTO ten VALUES (1), (2), (3), (4), (5), (6), (7), (8), (9), (10);
-- A cursor with no WITH HOLD would not survive the statement that made it, so
-- PostgreSQL refuses to open one outside a block.
DECLARE nope CURSOR FOR SELECT * FROM ten;
FETCH 1 FROM nope;
-- Directional fetches deliver every row they pass over.
BEGIN;
DECLARE c SCROLL CURSOR FOR SELECT * FROM ten ORDER BY g;
FETCH 3 c;
FETCH BACKWARD 1 c;
FETCH NEXT c;
FETCH PRIOR FROM c;
FETCH FORWARD 2 IN c;
FETCH FORWARD c;
FETCH BACKWARD c;
-- Omitting the direction means NEXT; FROM/IN are noise words.
FETCH c;
FETCH FROM c;
-- ABSOLUTE / RELATIVE land on a single row however far they travel, and a
-- negative count reads from the end.
FETCH ABSOLUTE 4 c;
FETCH RELATIVE -2 c;
FETCH ABSOLUTE -1 c;
FETCH FIRST c;
FETCH LAST c;
-- A zero count re-reads the row the cursor is already on.
FETCH RELATIVE 0 c;
FETCH 0 c;
-- ABSOLUTE 0 is the gap before the first row: legal, and empty.
FETCH ABSOLUTE 0 c;
-- ALL drains from wherever the cursor stands; a second one is empty.
FETCH ALL c;
FETCH ALL c;
-- Both ends are positions, not lost rows.
FETCH PRIOR c;
FETCH BACKWARD ALL c;
FETCH BACKWARD 1 c;
-- A negative count reverses the direction word.
FETCH 3 c;
FETCH -2 c;
FETCH BACKWARD -1 c;
-- MOVE is FETCH without the rows. The harness does not echo command tags, so
-- each move is pinned by the row the next FETCH lands on.
MOVE ABSOLUTE 0 c;
MOVE 3 c;
FETCH c;
MOVE ALL c;
FETCH c;
FETCH PRIOR c;
MOVE BACKWARD ALL c;
FETCH c;
MOVE FORWARD 5 IN c;
FETCH c;
MOVE c;
FETCH c;
MOVE BACKWARD 2 c;
FETCH c;
COMMIT;
-- The block's cursors went with it.
FETCH 1 c;
-- A name is unique per session, and the clash aborts the block.
BEGIN;
DECLARE c CURSOR FOR SELECT * FROM ten;
DECLARE c CURSOR FOR SELECT * FROM ten;
ROLLBACK;
-- CLOSE takes one cursor or all of them, and a missing name is an error.
BEGIN;
DECLARE c1 CURSOR FOR SELECT * FROM ten;
DECLARE c2 CURSOR FOR SELECT * FROM ten;
CLOSE c1;
SELECT name FROM pg_cursors ORDER BY 1;
CLOSE ALL;
SELECT name FROM pg_cursors ORDER BY 1;
COMMIT;
CLOSE nosuch;
--
-- Names fold like every other unquoted identifier, and a quoted name is a
-- different cursor.
--
BEGIN;
DECLARE Mixed CURSOR FOR SELECT * FROM ten ORDER BY g;
SELECT name FROM pg_cursors ORDER BY 1;
FETCH 1 mixed;
FETCH 1 MIXED;
DECLARE MIXED CURSOR FOR SELECT 1;
ROLLBACK;
BEGIN;
DECLARE Mixed CURSOR FOR SELECT * FROM ten;
DECLARE "Mixed" CURSOR FOR SELECT * FROM ten;
-- Two distinct cursors. Not ordered by name: the two spellings differ only in
-- case, so the order would be the collation's business rather than the cursor
-- code's.
SELECT count(*) FROM pg_cursors;
SELECT name FROM pg_cursors WHERE name = 'Mixed';
CLOSE "Mixed";
CLOSE MiXeD;
SELECT count(*) FROM pg_cursors;
COMMIT;
--
-- NO SCROLL refuses a rewind *request*, which is not the same as a move that
-- happens to end up behind where it started.
--
BEGIN;
DECLARE ns NO SCROLL CURSOR FOR SELECT * FROM ten ORDER BY g;
FETCH 2 ns;
FETCH BACKWARD 1 ns;
COMMIT;
BEGIN;
DECLARE ns NO SCROLL CURSOR FOR SELECT * FROM ten ORDER BY g;
FETCH ABSOLUTE 2 ns;
FETCH ABSOLUTE 3 ns;
FETCH ABSOLUTE 1 ns;
COMMIT;
-- Backward at the very start moves nothing, and is still refused.
BEGIN;
DECLARE ns NO SCROLL CURSOR FOR SELECT * FROM ten ORDER BY g;
FETCH BACKWARD 1 ns;
COMMIT;
BEGIN;
DECLARE ns NO SCROLL CURSOR FOR SELECT * FROM ten ORDER BY g;
FETCH BACKWARD ALL ns;
COMMIT;
-- A negative ABSOLUTE seeks from the end, so it rewinds even though it lands
-- ahead of the cursor.
BEGIN;
DECLARE ns NO SCROLL CURSOR FOR SELECT * FROM ten ORDER BY g;
FETCH ABSOLUTE -1 ns;
COMMIT;
-- A zero-distance re-fetch is a step back and forward again: allowed while the
-- cursor rests in an end gap, refused once it is on a row.
BEGIN;
DECLARE ns NO SCROLL CURSOR FOR SELECT * FROM ten ORDER BY g;
FETCH RELATIVE 0 ns;
FETCH 0 ns;
FETCH ABSOLUTE 0 ns;
FETCH 1 ns;
FETCH RELATIVE 0 ns;
COMMIT;
BEGIN;
DECLARE ns NO SCROLL CURSOR FOR SELECT * FROM ten ORDER BY g;
FETCH FIRST ns;
FETCH FIRST ns;
COMMIT;
-- BACKWARD 0 names a direction, so it rewinds even where FETCH 0 does not.
BEGIN;
DECLARE ns NO SCROLL CURSOR FOR SELECT * FROM ten ORDER BY g;
FETCH BACKWARD 0 ns;
COMMIT;
--
-- WITH HOLD survives the commit of the block that declared it.
--
BEGIN;
DECLARE held SCROLL CURSOR WITH HOLD FOR SELECT * FROM ten ORDER BY g;
DECLARE plain CURSOR FOR SELECT * FROM ten ORDER BY g;
COMMIT;
FETCH 2 held;
FETCH 1 plain;
-- A later, unrelated rollback does not touch it: it belongs to no block now.
BEGIN;
ROLLBACK;
FETCH 1 held;
-- But a rollback does close everything the aborting block declared, holdable
-- or not.
BEGIN;
DECLARE doomed CURSOR WITH HOLD FOR SELECT * FROM ten;
ROLLBACK;
FETCH 1 doomed;
--
-- pg_cursors reflects the session's open cursors.
--
SELECT name, statement, is_holdable, is_binary, is_scrollable FROM pg_cursors ORDER BY 1;
BEGIN;
DECLARE scr SCROLL CURSOR FOR SELECT * FROM ten;
DECLARE nsc NO SCROLL CURSOR FOR SELECT * FROM ten;
SELECT name, statement, is_holdable, is_binary, is_scrollable FROM pg_cursors ORDER BY 1;
COMMIT;
SELECT name, is_holdable, is_scrollable FROM pg_cursors ORDER BY 1;
CLOSE ALL;
SELECT count(*) FROM pg_cursors;
--
-- A cursor reads the snapshot its DECLARE took, not the one its FETCH runs
-- under.
--
BEGIN;
DECLARE snap CURSOR FOR SELECT * FROM ten ORDER BY g;
INSERT INTO ten VALUES (11);
FETCH ALL snap;
ROLLBACK;
-- The count is a sign in front of an int, so the grammar rejects anything else.
-- The literal it names never carries the sign, and -2147483648 is out of range
-- even though the signed value would fit.
FETCH 1.5 held;
MOVE 2147483648 held;
FETCH -2147483649 held;
FETCH ABSOLUTE -2147483648 held;
FETCH RELATIVE 2147483648 held;
MOVE BACKWARD 1.5 held;
FETCH $1 FROM held;
CLOSE ALL;
--
-- CLOSE counts as a query for the "SET TRANSACTION before any query" rule,
-- unlike FETCH and MOVE, which reuse the snapshot their DECLARE took.
--
BEGIN;
CLOSE ALL;
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;
ROLLBACK;
BEGIN;
DECLARE h2 CURSOR WITH HOLD FOR SELECT * FROM ten;
COMMIT;
BEGIN;
FETCH 1 h2;
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;
ROLLBACK;
BEGIN;
MOVE 1 h2;
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;
ROLLBACK;
CLOSE ALL;
DROP TABLE ten;
