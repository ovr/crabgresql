--
-- PG_SETTINGS
--
-- Never `SELECT * FROM pg_settings`: the row set is crabgresql's curated GUC
-- table, and every parameter added would rewrite this file. The row *count*
-- canary lives in a unit test in guc.rs, where it fails at the definition site.
-- server_version/server_version_num are never projected either — they embed
-- CARGO_PKG_VERSION and churn every release.
--
-- Output hand-written, not copied: a stock PostgreSQL reports
-- source = 'configuration file' with a non-NULL sourcefile/sourceline for
-- TimeZone, DateStyle and client_encoding after initdb, where this server
-- reports default/NULL/NULL, and its TimeZone boots at GMT rather than UTC.
-- Everything else below matches PostgreSQL 18.4 verbatim.
--
-- all seventeen columns, in PostgreSQL's order, on one deterministic row
SELECT name, setting, unit, category, short_desc, extra_desc, context,
       vartype, source, min_val, max_val, enumvals, boot_val, reset_val,
       sourcefile, sourceline, pending_restart
  FROM pg_settings WHERE name = 'default_transaction_read_only';
-- the declared types the all-NULL columns above cannot show
SELECT pg_typeof(enumvals) AS enumvals_type,
       pg_typeof(sourceline) AS sourceline_type,
       pg_typeof(pending_restart) AS pending_type
  FROM pg_settings WHERE name = 'TimeZone';
-- ORDER BY name COLLATE "C", never bare: TimeZone and DateStyle sort
-- differently under C than under the default collation
SELECT name, vartype, context, source, min_val, max_val, enumvals
  FROM pg_settings
 WHERE name IN ('TimeZone', 'extra_float_digits',
                'default_transaction_isolation', 'client_encoding')
 ORDER BY name COLLATE "C";
-- PostgreSQL's own oddity, reproduced: client_encoding and server_encoding boot
-- at SQL_ASCII and are then overridden, so boot_val and setting disagree while
-- source stays 'default'
SELECT name, setting, boot_val, source
  FROM pg_settings
 WHERE name IN ('client_encoding', 'server_encoding')
 ORDER BY name COLLATE "C";
-- is_superuser is readable by name but appears in neither SHOW ALL nor
-- pg_settings, as PostgreSQL flags it GUC_NO_SHOW_ALL
SHOW is_superuser;
SELECT count(*) AS is_superuser_rows FROM pg_settings WHERE name = 'is_superuser';
-- source follows SET and RESET, and reset_val is what RESET would restore
SET extra_float_digits = 2;
SELECT name, setting, source, boot_val, reset_val
  FROM pg_settings WHERE name = 'extra_float_digits';
RESET extra_float_digits;
SELECT name, setting, source FROM pg_settings WHERE name = 'extra_float_digits';
-- PostgreSQL reports 'session' even when the assignment changes nothing
SET extra_float_digits = 1;
SELECT name, setting, source FROM pg_settings WHERE name = 'extra_float_digits';
RESET extra_float_digits;
-- GUCs are transactional, and so is the source column: a ROLLBACK that puts
-- `setting` back must put `source` back too
BEGIN;
SET extra_float_digits = 3;
SELECT name, setting, source FROM pg_settings WHERE name = 'extra_float_digits';
ROLLBACK;
SELECT name, setting, source FROM pg_settings WHERE name = 'extra_float_digits';
-- ...including for a parameter that is accepted and ignored, which has no value
-- to restore but still has a source
BEGIN;
SET DateStyle = 'ISO, MDY';
SELECT name, source FROM pg_settings WHERE name = 'DateStyle';
ROLLBACK;
SELECT name, source FROM pg_settings WHERE name = 'DateStyle';
-- a rejected SET leaves source alone
SET extra_float_digits = 99;
SELECT name, setting, source FROM pg_settings WHERE name = 'extra_float_digits';
-- the view and current_setting() read the same table, so they cannot disagree
SELECT count(*) = 1 AS agrees
  FROM pg_settings
 WHERE name = 'TimeZone' AND setting = current_setting('TimeZone');
-- every row PostgreSQL fills only for one vartype is filled only there
SELECT count(*) AS misplaced_bounds
  FROM pg_settings
 WHERE (min_val IS NOT NULL) <> (vartype IN ('integer', 'real'));
SELECT count(*) AS misplaced_enumvals
  FROM pg_settings
 WHERE (enumvals IS NOT NULL) <> (vartype = 'enum');
-- no configuration file exists, so these three are constants
SELECT count(*) AS with_a_source_file
  FROM pg_settings
 WHERE sourcefile IS NOT NULL OR sourceline IS NOT NULL OR pending_restart;
--
-- SET LOCAL and SET are two levels, not one
--
-- PostgreSQL keeps a session value and a local value per parameter, and the two
-- can be assigned in either order in one block. Every sequence below was diffed
-- against PostgreSQL 18.4 and matches.
--
-- a SET LOCAL then a plain SET: the plain SET wins, and COMMIT keeps it
BEGIN;
SET LOCAL extra_float_digits = 3;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
SET extra_float_digits = 2;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
COMMIT;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
RESET extra_float_digits;
-- a plain SET then a SET LOCAL: COMMIT unmasks the session value
BEGIN;
SET extra_float_digits = 2;
SET LOCAL extra_float_digits = 3;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
COMMIT;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
RESET extra_float_digits;
-- ...and ROLLBACK discards both levels
BEGIN;
SET extra_float_digits = 2;
SET LOCAL extra_float_digits = 3;
ROLLBACK;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
-- two SET LOCALs mask one session value, which COMMIT restores
SET extra_float_digits = 0;
BEGIN;
SET LOCAL extra_float_digits = 3;
SET LOCAL extra_float_digits = 2;
COMMIT;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
RESET extra_float_digits;
-- a SET LOCAL alone never manufactures a source: COMMIT restores the pre-block
-- one, and so does ROLLBACK
BEGIN;
SET LOCAL extra_float_digits = 3;
COMMIT;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
-- ...including for a parameter with no value to restore. The two DateStyle rows
-- below read 'configuration file' on a stock PostgreSQL and 'default' here, the
-- divergence the file header describes; what is being checked is that the
-- source comes back unchanged, whichever it started as.
BEGIN;
SET LOCAL DateStyle = 'ISO, MDY';
COMMIT;
SELECT name, source FROM pg_settings WHERE name = 'DateStyle';
BEGIN;
SET LOCAL DateStyle = 'ISO, MDY';
ROLLBACK;
SELECT name, source FROM pg_settings WHERE name = 'DateStyle';
-- PostgreSQL's own oddity, reproduced: it stores no source for a masked value
-- and restores it as 'session' outright, so a RESET that a SET LOCAL then masks
-- comes back reporting 'session' over the boot value.
SET extra_float_digits = 0;
BEGIN;
SET LOCAL extra_float_digits = 3;
RESET extra_float_digits;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
SET LOCAL extra_float_digits = 2;
COMMIT;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
RESET extra_float_digits;
-- SET x = DEFAULT is RESET x, so the source goes back to 'default' too
SET extra_float_digits = 2;
SET extra_float_digits = DEFAULT;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
SET extra_float_digits = 2;
BEGIN;
SET extra_float_digits = DEFAULT;
COMMIT;
SELECT setting, source FROM pg_settings WHERE name = 'extra_float_digits';
RESET extra_float_digits;
-- SET SESSION CHARACTERISTICS marks only the parameters it names
SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY;
SELECT name, setting, source FROM pg_settings
 WHERE name LIKE 'default_transaction%' ORDER BY name COLLATE "C";
SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ;
SELECT name, setting, source FROM pg_settings
 WHERE name LIKE 'default_transaction%' ORDER BY name COLLATE "C";
-- ...and both are put back, or every statement after this one runs read-only
RESET ALL;
SELECT name, setting, source FROM pg_settings
 WHERE name LIKE 'default_transaction%' ORDER BY name COLLATE "C";
--
-- transaction_isolation and transaction_read_only live in the transaction
--
-- PostgreSQL's two transaction-scoped parameters: the value is the open block's
-- rather than the session's, which is why `source` starts at `override`, `RESET`
-- is refused, and what `SHOW` prints follows the block. Every statement below
-- was diffed against PostgreSQL 18.4.
--
SELECT name, setting, category, short_desc, context, vartype, source,
       enumvals, boot_val, reset_val
  FROM pg_settings
 WHERE name IN ('transaction_isolation', 'transaction_read_only')
 ORDER BY name COLLATE "C";
-- the grammar's own spelling of the first one, column name included
SHOW TRANSACTION ISOLATION LEVEL;
SHOW transaction_isolation;
SHOW transaction_read_only;
-- outside a block both follow the defaults a new transaction would inherit
SET default_transaction_isolation = 'repeatable read';
SHOW TRANSACTION ISOLATION LEVEL;
RESET default_transaction_isolation;
-- a mode a BEGIN names is 'session' while the block lasts and 'override' after
BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY;
SHOW TRANSACTION ISOLATION LEVEL;
SELECT name, setting, source FROM pg_settings
 WHERE name IN ('transaction_isolation', 'transaction_read_only')
 ORDER BY name COLLATE "C";
COMMIT;
SELECT name, setting, source FROM pg_settings
 WHERE name IN ('transaction_isolation', 'transaction_read_only')
 ORDER BY name COLLATE "C";
-- ...where a SET TRANSACTION's mark survives the COMMIT, as any plain SET's does
BEGIN;
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;
SELECT name, setting, source FROM pg_settings
 WHERE name = 'transaction_isolation';
COMMIT;
-- the value went with the block; only the source stayed behind
SELECT name, setting, source FROM pg_settings
 WHERE name = 'transaction_isolation';
-- ...and a ROLLBACK takes the source back too
BEGIN;
SET transaction_read_only = on;
SELECT name, setting, source FROM pg_settings WHERE name = 'transaction_read_only';
ROLLBACK;
SELECT name, setting, source FROM pg_settings WHERE name = 'transaction_read_only';
-- neither can be reset: PostgreSQL flags them GUC_NO_RESET
RESET transaction_isolation;
SET transaction_read_only = DEFAULT;
-- ...but RESET ALL skips them rather than raising
RESET ALL;
SELECT name, setting, source FROM pg_settings
 WHERE name IN ('transaction_isolation', 'transaction_read_only')
 ORDER BY name COLLATE "C";
