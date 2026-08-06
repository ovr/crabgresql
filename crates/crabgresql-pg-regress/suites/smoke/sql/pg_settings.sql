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
