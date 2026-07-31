--
-- COPY ... FROM STDIN
-- Smoke test for the text and CSV copy-in sub-protocol: tab-delimited and CSV
-- rows, the \N NULL marker and backslash escapes, an omitted column taking its
-- default, and char(n) blank-padding on load.
--
CREATE TABLE cp (a integer, b text, c integer DEFAULT 7);
-- Basic text format: TAB-delimited, one row per line.
COPY cp FROM stdin;
1	one	10
2	two	20
\.
SELECT * FROM cp ORDER BY a;
-- Column list: the unlisted column (c) takes its default; \N is a SQL NULL.
COPY cp (a, b) FROM stdin;
3	three
4	\N
\.
SELECT * FROM cp ORDER BY a;
-- Backslash escapes: \\ is a literal backslash and \061 is octal for '1'.
CREATE TABLE esc (v text);
COPY esc FROM stdin;
c\\d
x\061y
\.
SELECT v, octet_length(v) FROM esc ORDER BY v;
-- A \t escape decodes to a single tab byte (octet_length 3, not 4 literal chars).
CREATE TABLE esctab (v text);
COPY esctab FROM stdin;
a\tb
\.
SELECT octet_length(v) AS n FROM esctab;
-- CSV with a header line and quoted fields ("" is one literal quote).
CREATE TABLE cs (a integer, b text);
COPY cs FROM stdin WITH (FORMAT csv, HEADER);
col_a,col_b
1,"a,b"
2,"she ""said"""
\.
SELECT * FROM cs ORDER BY a;
-- CSV honoring a custom delimiter and NULL string.
CREATE TABLE cd (a integer, b text);
COPY cd FROM stdin WITH (FORMAT csv, DELIMITER '|', NULL 'NULL');
1|hello
2|NULL
\.
SELECT a, b FROM cd ORDER BY a;
-- char(n) blank-pads on load; octet_length counts the padding.
CREATE TABLE ch (a char(5));
COPY ch FROM stdin;
ab
\.
SELECT octet_length(a) AS len FROM ch;

-- Server-side COPY FROM a file, addressed the way the upstream corpus does:
-- the harness exports PG_ABS_SRCDIR, `\set` concatenates it with the relative
-- data path, and `:'filename'` interpolates the result as a quoted literal.
\getenv abs_srcdir PG_ABS_SRCDIR
\set filename :abs_srcdir '/data/copy_file.data'
CREATE TABLE cf (a integer, b text);
COPY cf FROM :'filename';
SELECT a, b FROM cf ORDER BY a;
