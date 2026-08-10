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
