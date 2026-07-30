--
-- WINDOW FUNCTIONS
-- PARTITION BY / ORDER BY, the ranking trio (row_number/rank/dense_rank) and
-- the standard aggregates in window mode, all over the default frame
-- (RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW). Covers peer-group
-- semantics — which is what makes a running aggregate give tied rows the same
-- value — NULL partitioning, multiple specs, the WINDOW clause, windows over a
-- grouped aggregate, and the errors PG reports for a misplaced window call.
--
-- Every query carries an explicit ORDER BY: a window query with no ORDER BY of
-- its own returns rows in the last-evaluated spec's sort order, and rows that
-- tie on every key then have no defined order to pin.
--
CREATE TABLE empsalary (depname text, empno integer, salary integer);
INSERT INTO empsalary VALUES
  ('develop', 7, 4200),
  ('develop', 9, 4500),
  ('develop', 10, 5200),
  ('develop', 11, 5200),
  ('personnel', 2, 3900),
  ('personnel', 5, 3500),
  ('sales', 1, 5000),
  ('sales', 3, 4800),
  ('sales', 4, 4800);
-- PARTITION BY alone: the aggregate is the whole partition, repeated per row
SELECT depname, empno, sum(salary) OVER (PARTITION BY depname) FROM empsalary ORDER BY depname, empno;
-- ORDER BY alone: one partition, and the aggregate is a running total
SELECT empno, sum(salary) OVER (ORDER BY empno) FROM empsalary ORDER BY empno;
-- both
SELECT depname, empno, sum(salary) OVER (PARTITION BY depname ORDER BY empno) FROM empsalary ORDER BY depname, empno;
-- the ranking trio over deliberate ties: rank skips after a tie, dense_rank
-- does not, and row_number never ties
SELECT depname, salary, rank() OVER w, dense_rank() OVER w, row_number() OVER w
  FROM empsalary WINDOW w AS (PARTITION BY depname ORDER BY salary)
  ORDER BY depname, salary, empno;
-- THE peer-group semantic: the default RANGE frame ends at the current row's
-- LAST peer, so tied rows share a running total. `develop` has two rows at
-- 5200, and both see 19100 rather than 13900 and 19100.
SELECT depname, salary, sum(salary) OVER (PARTITION BY depname ORDER BY salary)
  FROM empsalary ORDER BY depname, salary, empno;
-- with no ORDER BY every row is a peer of every other, so rank() is 1
-- everywhere and the aggregate is the partition total
SELECT depname, rank() OVER (PARTITION BY depname), count(*) OVER (PARTITION BY depname)
  FROM empsalary ORDER BY depname, empno;
-- min/max/avg/string_agg in window mode
SELECT empno, min(salary) OVER w, max(salary) OVER w, avg(salary) OVER w
  FROM empsalary WINDOW w AS (ORDER BY empno) ORDER BY empno;
SELECT empno, string_agg(depname, ',') OVER (ORDER BY empno) FROM empsalary ORDER BY empno;
-- two specs in one query
SELECT empno, rank() OVER (ORDER BY salary), sum(salary) OVER (PARTITION BY depname)
  FROM empsalary ORDER BY empno;
-- two named windows with identical bodies collapse to one step
SELECT empno, rank() OVER w1, sum(salary) OVER w2
  FROM empsalary WINDOW w1 AS (ORDER BY empno), w2 AS (ORDER BY empno)
  ORDER BY empno;
-- OVER w with an inline ORDER BY added to a base that has none
SELECT empno, rank() OVER (w ORDER BY salary)
  FROM empsalary WINDOW w AS (PARTITION BY depname) ORDER BY depname, salary, empno;
-- a window over a grouped aggregate: the inner sum is per group, the outer one
-- runs over the groups
SELECT depname, sum(salary), sum(sum(salary)) OVER (ORDER BY depname)
  FROM empsalary GROUP BY depname ORDER BY depname;
-- a window in ORDER BY, which rides a hidden column
SELECT empno FROM empsalary ORDER BY rank() OVER (ORDER BY salary), empno;
-- the default frame written out longhand means the same thing
SELECT empno, sum(salary) OVER (ORDER BY empno RANGE UNBOUNDED PRECEDING) FROM empsalary ORDER BY empno;
-- NULLs: a NULL partition key groups with the other NULLs, and NULLs in the
-- window ORDER BY are their own peer group, placed per the direction's default
CREATE TABLE wnull (k integer, v integer);
INSERT INTO wnull VALUES (NULL, 1), (NULL, 2), (1, 4), (1, 8);
SELECT k, v, count(*) OVER (PARTITION BY k) FROM wnull ORDER BY k NULLS FIRST, v;
SELECT k, v, rank() OVER (ORDER BY k), sum(v) OVER (ORDER BY k) FROM wnull ORDER BY k NULLS FIRST, v;
SELECT k, v, rank() OVER (ORDER BY k DESC) FROM wnull ORDER BY k NULLS FIRST, v;
-- a single-row partition, and an empty table
SELECT k, rank() OVER (PARTITION BY v ORDER BY k) FROM wnull ORDER BY v;
CREATE TABLE wempty (a integer);
SELECT a, rank() OVER (ORDER BY a), count(*) OVER () FROM wempty ORDER BY a;
-- a FROM-less window runs over the one virtual row
SELECT row_number() OVER (), rank() OVER (ORDER BY 1);
-- WHERE runs before windows do, so it filters the rows the window sees
SELECT empno, count(*) OVER () FROM empsalary WHERE salary > 4500 ORDER BY empno;
-- a window with LIMIT: the limit applies after the window and its sort
SELECT empno, rank() OVER (ORDER BY salary DESC) FROM empsalary ORDER BY 2, empno LIMIT 3;
-- DISTINCT applies to the window's output
SELECT DISTINCT sum(salary) OVER (PARTITION BY depname) FROM empsalary ORDER BY 1;
-- errors: every clause evaluated before windows are
SELECT 1 FROM empsalary WHERE rank() OVER () > 1;
SELECT depname FROM empsalary GROUP BY depname HAVING rank() OVER () > 1;
SELECT depname FROM empsalary GROUP BY depname, rank() OVER ();
SELECT rank() OVER (PARTITION BY rank() OVER ()) FROM empsalary;
-- errors: nesting, and an aggregate over a window
SELECT sum(rank() OVER ()) OVER () FROM empsalary;
SELECT sum(sum(salary) OVER ()) FROM empsalary;
-- errors: forms PG does not implement
SELECT sum(DISTINCT salary) OVER () FROM empsalary;
-- errors: a window function needs an OVER clause, and OVER needs a window
-- function or an aggregate
SELECT rank() FROM empsalary;
SELECT abs(salary) OVER () FROM empsalary;
-- errors: the WINDOW clause's copy rules
SELECT rank() OVER w FROM empsalary;
SELECT rank() OVER (w PARTITION BY depname) FROM empsalary WINDOW w AS (ORDER BY salary);
SELECT rank() OVER (w ORDER BY empno) FROM empsalary WINDOW w AS (ORDER BY salary);
SELECT rank() OVER (w) FROM empsalary WINDOW w AS (ORDER BY salary ROWS UNBOUNDED PRECEDING);
SELECT rank() OVER () FROM empsalary WINDOW w AS (ORDER BY salary), w AS (ORDER BY empno);
-- but `OVER w` is a reference, not a copy: it inherits the base's frame rather
-- than being refused for having one, so it reaches the unimplemented frame path
SELECT rank() OVER w FROM empsalary WINDOW w AS (ORDER BY salary ROWS UNBOUNDED PRECEDING);
-- errors: explicit frames are not implemented yet
SELECT sum(salary) OVER (ORDER BY empno ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM empsalary;
SELECT sum(salary) OVER (ORDER BY empno RANGE UNBOUNDED PRECEDING EXCLUDE TIES) FROM empsalary;
DROP TABLE empsalary;
DROP TABLE wnull;
DROP TABLE wempty;
