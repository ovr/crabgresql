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

-- HEADER match: the first line is a header AND its names are checked against
-- the columns the statement names, in that order.
CREATE TABLE hm (a integer, b text);
COPY hm FROM stdin WITH (FORMAT csv, HEADER match);
a,b
1,one
\.
SELECT a, b FROM hm ORDER BY a;
COPY hm FROM stdin WITH (FORMAT csv, HEADER match);
b,a
2,two
\.
COPY hm (b, a) FROM stdin WITH (FORMAT csv, HEADER match);
b,a
two,2
\.
SELECT a, b FROM hm ORDER BY a;
COPY hm FROM stdin WITH (FORMAT csv, HEADER match);
a
3
\.

-- COPY ... FREEZE. The rows are stamped visible-to-everyone, which is only safe
-- where a rollback discards the storage — so the table must have been truncated
-- in this same transaction. Outside a block it never has been.
CREATE TABLE vistest (a text);
COPY vistest FROM stdin CSV FREEZE;
a1
b
\.
BEGIN;
TRUNCATE vistest;
COPY vistest FROM stdin CSV FREEZE;
a2
b
\.
SELECT * FROM vistest ORDER BY a;
COMMIT;
SELECT * FROM vistest ORDER BY a;
-- A rollback still loses the frozen rows: the truncated file goes with it.
BEGIN;
TRUNCATE vistest;
COPY vistest FROM stdin WITH (FORMAT csv, FREEZE ON);
x
y
\.
ROLLBACK;
SELECT * FROM vistest ORDER BY a;
-- FREEZE OFF is an ordinary load and needs no truncate.
COPY vistest FROM stdin WITH (FORMAT csv, FREEZE OFF);
z
\.
SELECT * FROM vistest ORDER BY a;
-- A boolean option takes 1/0 and the quoted spellings too, and anything else is
-- rejected by name rather than as a bare syntax error.
COPY vistest FROM stdin WITH (FORMAT csv, FREEZE 0);
w
\.
COPY vistest FROM stdin WITH (FORMAT csv, FREEZE 'off');
u
\.
COPY vistest FROM stdin WITH (FORMAT csv, FREEZE "off");
t
\.
COPY vistest FROM stdin WITH (FORMAT csv, FREEZE -0);
v
\.
SELECT * FROM vistest ORDER BY a;
-- The rejections below name a file rather than stdin only so this script stays
-- readable: the statement never gets far enough to open it, and psql would not
-- enter copy mode for a statement the server refused anyway.
COPY vistest FROM '/dev/null' WITH (FREEZE yes);
-- A repeated option is refused with the caret on the second occurrence.
COPY vistest FROM '/dev/null' (format csv, FORMAT CSV);
COPY vistest FROM '/dev/null' (freeze off, freeze on);
COPY vistest FROM '/dev/null' (delimiter ',', delimiter ',');
-- The rule is about options, not about parentheses: it covers the pre-9.0 list
-- and the sub-options of its CSV item too.
COPY vistest FROM '/dev/null' CSV FREEZE FREEZE;
COPY vistest FROM '/dev/null' CSV HEADER HEADER;
COPY vistest FROM '/dev/null' DELIMITER ',' DELIMITER ',';
-- And the two spellings are alternatives, so carrying both is a syntax error
-- rather than a redundant option.
COPY vistest FROM '/dev/null' (FREEZE OFF) CSV FREEZE;

-- A field that its column's input function rejects aborts the whole load, and
-- the message is the input function's own. COPY has no statement text to point
-- at, so unlike the same literal in an INSERT there is no `LINE n: ... ^`.
-- (PostgreSQL adds a `CONTEXT: COPY <table>, line n, column c: "..."` line here
-- that this server does not yet emit.)
CREATE TABLE bad (a integer, b text);
COPY bad FROM stdin;
1	one
zzz	two
\.
SELECT count(*) AS loaded FROM bad;
-- Wrong arity, both directions.
COPY bad FROM stdin;
1	one	extra
\.
COPY bad FROM stdin;
1
\.
-- Rows are read in order, so the first bad row wins even when a later row is
-- bad in a different way.
COPY bad FROM stdin;
1	one
nope	two
1	one	extra
\.
-- Same, for a column whose value the binder cannot fold without a session: the
-- literal is still checked where it is read, so a bad `timestamptz` in row 1
-- beats an arity error in row 2 rather than the load reporting them backwards.
CREATE TABLE ord (z timestamptz, b text);
COPY ord FROM stdin;
not-a-time	one
2020-01-01	two	extra
\.
-- The column's length typmod applies in assignment context: an over-long
-- varchar errors rather than truncating, while `name` always truncates.
CREATE TABLE lens (v varchar(3), n name);
COPY lens (v) FROM stdin;
abcd
\.
COPY lens (n) FROM stdin;
0123456789012345678901234567890123456789012345678901234567890123456789
\.
SELECT octet_length(n) AS name_len FROM lens WHERE n IS NOT NULL;
-- numeric(p,s) rounds to the scale, and overflows once the integer part no
-- longer fits.
CREATE TABLE num (v numeric(5,2));
COPY num FROM stdin;
1.005
-1.005
\.
SELECT v FROM num ORDER BY v;
COPY num FROM stdin;
12345.6
\.
-- An enum column takes its labels by name, through the type catalog.
CREATE TYPE cpmood AS ENUM ('sad', 'ok', 'happy');
CREATE TABLE en (m cpmood);
COPY en FROM stdin;
ok
\.
SELECT m FROM en;
COPY en FROM stdin;
elated
\.
-- A NULL marker in a length-modified column is a NULL, not a padded blank: it
-- never reaches the typmod.
CREATE TABLE nul (c char(5));
COPY nul FROM stdin;
\N
\.
SELECT c IS NULL AS is_null, octet_length(c) AS len FROM nul;
-- `now` reads the transaction clock, which only the executing session holds.
CREATE TABLE clk (t timestamp, z timestamptz);
BEGIN;
COPY clk FROM stdin;
now	now
\.
SELECT t = transaction_timestamp()::timestamp AS t_is_xact,
       z = transaction_timestamp() AS z_is_xact FROM clk;
COMMIT;
-- An interval literal whose meaning IntervalStyle changes is read under the
-- session's style.
CREATE TABLE iv (v interval);
SET IntervalStyle = 'sql_standard';
COPY iv FROM stdin;
-1 2:03:04
\.
SELECT v FROM iv;
-- COPY and INSERT must read the same text into the same value. Asserted on
-- interval[] because array elements take a different route through the input
-- functions than the scalar above.
CREATE TABLE ivd (a interval[]);
COPY ivd FROM stdin;
{-1 2:03:04}
\.
INSERT INTO ivd VALUES ('{-1 2:03:04}');
SELECT count(*) AS rows, count(DISTINCT a) AS distinct_values FROM ivd;
RESET IntervalStyle;

-- Server-side COPY FROM a file, addressed the way the upstream corpus does:
-- the harness exports PG_ABS_SRCDIR, `\set` concatenates it with the relative
-- data path, and `:'filename'` interpolates the result as a quoted literal.
\getenv abs_srcdir PG_ABS_SRCDIR
\set filename :abs_srcdir '/data/copy_file.data'
CREATE TABLE cf (a integer, b text);
COPY cf FROM :'filename';
SELECT a, b FROM cf ORDER BY a;
