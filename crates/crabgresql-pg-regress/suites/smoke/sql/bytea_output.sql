--
-- bytea_output: which of byteaout's two renderings a bytea takes on the way
-- out. Output only — byteain reads both forms whatever the setting is, which
-- is what lets an escape-rendered value be pasted back into a hex session.
--
SHOW bytea_output;
-- the boundaries of the printable range: 0x7e is the last byte that prints
-- verbatim and 0x7f the first that does not, and a backslash doubles
SELECT decode('0001027f807e5c2027225a', 'hex') AS hex_form;
SET bytea_output TO escape;
SELECT decode('0001027f807e5c2027225a', 'hex') AS escape_form;
-- the cast to text is the same output function
SELECT decode('00615c', 'hex')::text AS as_text;
-- ...and so is array element rendering, which quotes and doubles on top of it
SELECT array[decode('0061', 'hex'), decode('5c', 'hex')] AS as_array;
-- concat() reaches the output function too
SELECT concat('<', decode('0061', 'hex'), '>') AS concatenated;
-- the setting does not touch input: both spellings still read as the same bytes
SELECT '\x00615c'::bytea = decode('00615c', 'hex') AS hex_literal_reads,
       '\000a\\'::bytea = decode('00615c', 'hex') AS escape_literal_reads;
-- encode()/decode() name their own format and ignore the GUC
SELECT encode(decode('00615c', 'hex'), 'hex') AS encoded_hex;
-- the empty string is empty in escape form and a bare \x in hex form
SELECT decode('', 'hex') AS empty_escape;
RESET bytea_output;
SELECT decode('', 'hex') AS empty_hex;
-- the name and the value are both matched case-insensitively
SET BYTEA_OUTPUT TO 'ESCAPE';
SHOW bytea_output;
RESET bytea_output;
-- ...and nothing more: padding is part of the value
SET bytea_output TO ' hex ';
SET bytea_output TO bogus;
-- the setting is transactional, and SET LOCAL does not outlive the block
BEGIN;
SET bytea_output TO escape;
SELECT decode('61', 'hex') AS inside;
ROLLBACK;
SELECT decode('61', 'hex') AS after_rollback;
BEGIN;
SET LOCAL bytea_output TO escape;
SELECT decode('61', 'hex') AS local_inside;
COMMIT;
SELECT decode('61', 'hex') AS after_commit;
-- pg_settings agrees with SHOW, and reports PostgreSQL's own metadata
SELECT name, setting, category, vartype, boot_val, enumvals, context
  FROM pg_settings WHERE name = 'bytea_output';
SET bytea_output TO escape;
SELECT name, setting, source, reset_val FROM pg_settings
 WHERE name = 'bytea_output';
SELECT current_setting('bytea_output') AS via_current_setting;
RESET bytea_output;
--
-- encode(bytea, 'escape') is a DIFFERENT rule and does not follow the GUC
--
-- PostgreSQL escapes only NUL, the backslash and the high-bit bytes there,
-- passing the C0 controls and 0x7f through untouched, where byteaout escapes
-- everything outside 0x20..0x7e. The two spell the same input differently, and
-- the setting moves only the second.
SELECT encode(decode('00011f7f80ff5c41', 'hex'), 'escape') = decode('00011f7f80ff5c41', 'hex')::text
       AS encode_differs_from_byteaout;
SET bytea_output TO escape;
SELECT encode(decode('00011f7f80ff5c41', 'hex'), 'escape') = decode('00011f7f80ff5c41', 'hex')::text
       AS still_differs_under_escape;
-- both spellings decode back to the same bytes, which is what makes the choice
-- an output-only one
SELECT decode(encode(decode('00011f7f80ff5c41', 'hex'), 'escape'), 'escape')
         = decode('00011f7f80ff5c41', 'hex') AS encode_roundtrips;
RESET bytea_output;
--
-- the deparse paths run the datum through byteaout, so they follow the reader
--
CREATE TABLE bo_default (b bytea DEFAULT '\x27615c');
SELECT pg_get_expr(d.adbin, d.adrelid) AS hex_reader
  FROM pg_attrdef d JOIN pg_class c ON c.oid = d.adrelid
 WHERE c.relname = 'bo_default';
SET bytea_output TO escape;
-- the 0x27 byte prints verbatim in escape form and is then doubled inside the
-- SQL literal, so this is also the quoting test
SELECT pg_get_expr(d.adbin, d.adrelid) AS escape_reader
  FROM pg_attrdef d JOIN pg_class c ON c.oid = d.adrelid
 WHERE c.relname = 'bo_default';
RESET bytea_output;
DROP TABLE bo_default;
-- `pg_get_constraintdef` shares the re-render, but a CHECK cannot reach it for
-- a bytea today: the write-time deparse labels a bare literal `::text` rather
-- than resolving its type from the comparison, so `CHECK (b <> '\x0061')` is
-- stored as `'\x0061'::text` and never matches the bytea arm. That gap is
-- type-general — a `date` constant in a CHECK is mislabelled the same way — so
-- it is not pinned here; see the note on `bytea_constant` in ruleutils.rs.
