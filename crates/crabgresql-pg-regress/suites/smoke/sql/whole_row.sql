--
-- Whole-row references
-- `t.*` is a composite value where a larger expression consumes it, and an
-- *expansion* where the select list holds nothing else.
--
CREATE TABLE wr (a int4, b text, c bool);
INSERT INTO wr VALUES (1, 'x y', true), (2, NULL, false), (3, '', NULL), (4, 'q"r\s', true);
-- record_out: a NULL field prints as nothing, an empty string has to quote so
-- the two stay apart, and the separators/whitespace/backslashes escape.
SELECT greatest(wr.*) FROM wr ORDER BY a;
SELECT coalesce(wr.*) AS whole FROM wr ORDER BY a LIMIT 1;
-- A select-list item that *is* the star expands, however many parentheses it
-- wears and whatever it is aliased to.
SELECT wr.* FROM wr ORDER BY a LIMIT 1;
SELECT wr.* AS ignored FROM wr ORDER BY a LIMIT 1;
SELECT (wr.*) FROM wr ORDER BY a LIMIT 1;
SELECT ((wr.*)) AS ignored FROM wr ORDER BY a LIMIT 1;
SELECT (wr.*), 1 FROM wr ORDER BY a LIMIT 1;
-- The star names a FROM item by its alias, so an alias renames it.
SELECT greatest(w.*) FROM wr w ORDER BY a LIMIT 1;
-- A qualifier no FROM item declares is 42P01, as it is for a qualified column.
SELECT greatest(nope.*) FROM wr;
-- A system column is not part of the row, exactly as it is not part of `t.*`.
CREATE TABLE wr_sys (a int4);
INSERT INTO wr_sys VALUES (7);
SELECT greatest(wr_sys.*) FROM wr_sys;
DROP TABLE wr_sys;
-- Field-wise ordering (`record_cmp`): the first differing field decides, a NULL
-- field sorts last.
SELECT greatest(wr.*) FROM wr ORDER BY 1 DESC LIMIT 1;
SELECT DISTINCT greatest(wr.*) AS rows FROM wr ORDER BY 1;
-- An aggregate takes a whole-row reference as an ordinary argument: only the
-- bare `*` is the row wildcard, so `count(t.*)`, `count(DISTINCT t.*)` and
-- `min(t.*)` all bind. A record is never NULL as a datum, so `count(t.*)`
-- matches `count(*)` even for an all-NULL row.
SELECT count(wr.*) AS rows, count(DISTINCT wr.*) AS distinct_rows,
       count(*) AS star FROM wr;
SELECT min(wr.*) AS smallest FROM wr;
-- `IS NULL` on a row asks about *every* field, and `IS NOT NULL` is not its
-- negation: a mixed row answers false to both.
CREATE TABLE wr_null (a int4, b text);
INSERT INTO wr_null VALUES (1, 'x'), (2, NULL), (NULL, NULL);
SELECT a, greatest(wr_null.*) IS NULL AS is_null,
       greatest(wr_null.*) IS NOT NULL AS is_not_null
  FROM wr_null ORDER BY a NULLS LAST;
-- The null-extended side of an outer join is NULL as a whole row, which the
-- same rule answers. Divergence: PostgreSQL *prints* it as NULL where we print
-- the all-NULL row `()` — telling the two apart needs the join's
-- null-extension flag on the value, which nothing here carries.
SELECT greatest(n.*) IS NULL AS extended_is_null, greatest(n.*)::text AS printed
  FROM wr_null k LEFT JOIN wr_null n ON (false) LIMIT 1;
-- A `record` cannot be stored: it is a pseudo-type, and the on-disk codec has
-- no tag for a composite. Divergence: PostgreSQL accepts this, giving the
-- column the relation's own named composite type — nothing here declares one.
-- (Left uncommitted on both sides, so the DROP below behaves the same way.)
BEGIN;
CREATE TABLE wr_ctas AS SELECT greatest(wr_null.*) FROM wr_null;
ROLLBACK;
DROP TABLE wr_null;
DROP TABLE wr;
