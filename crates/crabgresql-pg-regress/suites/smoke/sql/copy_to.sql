--
-- COPY ... TO STDOUT
-- Smoke test for the copy-out sub-protocol: text and CSV escaping, HEADER, a
-- written column list, the query form, FORCE_QUOTE, and the option refusals.
-- psql writes copy-out rows raw, so what follows the statement is the payload
-- itself rather than a result table.
--
CREATE TABLE co (a integer, b text, c text);
INSERT INTO co VALUES
  (1, 'plain', 'has,comma'),
  (2, E'tab\there', NULL),
  (3, 'has"quote', E'nl\nhere'),
  (4, E'back\\slash', ' lead trail ');
-- Text format: TAB-delimited, backslash escapes, \N for NULL.
COPY co TO stdout;
-- A written column list emits those columns in that order, not the schema's.
COPY co (c, a) TO stdout;
-- HEADER writes the column names through the same escaping rules as data.
COPY co TO stdout (HEADER);
-- A custom delimiter is escaped in the data just as TAB is by default.
COPY co TO stdout (DELIMITER '|');
-- A custom NULL marker goes out verbatim.
COPY co TO stdout (NULL 'NUL');
-- CSV quotes only what it must: the delimiter, the quote, and newlines. A
-- backslash, a TAB and surrounding spaces are not triggers.
COPY co TO stdout (FORMAT csv);
COPY co TO stdout (FORMAT csv, HEADER);
-- The empty string collides with CSV's default NULL marker, so it is quoted;
-- under a non-empty marker it is not, and the marker's own value is.
COPY (SELECT ''::text UNION ALL SELECT 'NUL') TO stdout (FORMAT csv);
COPY (SELECT ''::text UNION ALL SELECT 'NUL') TO stdout (FORMAT csv, NULL 'NUL');
-- A lone \. would read back as the end-of-data marker, but only as a whole
-- line: single-column output quotes it, wider output does not.
COPY (SELECT '\.'::text) TO stdout (FORMAT csv);
COPY (SELECT '\.'::text, 'x'::text) TO stdout (FORMAT csv);
-- A custom quote and escape: the quote and the escape byte are each prefixed
-- with the escape byte inside a quoted field.
COPY co TO stdout (FORMAT csv, QUOTE '~', ESCAPE '\');
-- FORCE_QUOTE quotes data fields regardless of content, but never NULL and
-- never the header.
COPY co TO stdout (FORMAT csv, HEADER, FORCE_QUOTE *);
COPY co (a, b) TO stdout (FORMAT csv, FORCE_QUOTE (b));
-- The query form takes its column names from the query.
COPY (SELECT a AS n, upper(b) AS u FROM co WHERE a < 3 ORDER BY a) TO stdout (FORMAT csv, HEADER);
-- An empty result still writes the header.
COPY (SELECT 1 WHERE false) TO stdout (HEADER);
-- A round trip: feeding COPY TO's own output back through COPY FROM must
-- reproduce the table, so the second dump below is byte-identical to the first.
CREATE TABLE co2 (a integer, b text, c text);
COPY co2 FROM stdin;
1	plain	has,comma
2	tab\there	\N
3	has"quote	nl\nhere
4	back\\slash	 lead trail 
\.
COPY co2 TO stdout;
-- `COPY t TO` copies `SELECT * FROM ONLY t`: an inheritance child's rows are
-- the child's own to dump.
CREATE TABLE copar (a integer);
CREATE TABLE coch () INHERITS (copar);
INSERT INTO copar VALUES (1);
INSERT INTO coch VALUES (2);
COPY copar TO stdout;
COPY (SELECT a FROM copar ORDER BY a) TO stdout;
-- A view or a partitioned parent is refused by name: COPY (SELECT ...) is the
-- supported spelling for both.
CREATE VIEW cov AS SELECT a FROM co;
COPY cov TO stdout;
CREATE TABLE copt (a integer) PARTITION BY RANGE (a);
CREATE TABLE copt1 PARTITION OF copt FOR VALUES FROM (0) TO (10);
INSERT INTO copt VALUES (5);
COPY copt TO stdout;
COPY copt1 TO stdout;
COPY (SELECT a FROM copt) TO stdout;
-- Option refusals, each with its own message.
COPY co TO stdout (HEADER MATCH);
COPY co TO stdout (FORMAT csv, FORCE_NOT_NULL (a));
COPY co TO stdout (FORMAT csv, FORCE_NULL (a));
COPY co TO stdout (QUOTE '~');
COPY co TO stdout (FREEZE);
COPY co TO stdout (FORMAT csv, DELIMITER '"');
COPY co TO stdout (DELIMITER '|', NULL 'a|b');
COPY co TO stdout (FORMAT csv, NULL 'a"b');
COPY co TO stdout (FORMAT binary, DELIMITER '|');
COPY co TO stdout (FORMAT binary, HEADER);
COPY co TO stdout (FORMAT csv, FORCE_QUOTE (nosuch));
COPY nosuchtable TO stdout;
-- The pre-9.0 bare keywords are alternatives, not options that stack.
COPY co TO stdout CSV QUOTE '~' BINARY;
-- A COPY TO in an aborted transaction is refused like any other statement; a
-- read-only transaction, on the other hand, runs it.
BEGIN;
SELECT 1/0;
COPY co TO stdout;
ROLLBACK;
BEGIN READ ONLY;
COPY (SELECT 42) TO stdout;
COMMIT;
DROP VIEW cov;
DROP TABLE copt1;
DROP TABLE copt;
DROP TABLE coch;
DROP TABLE copar;
DROP TABLE co2;
DROP TABLE co;
