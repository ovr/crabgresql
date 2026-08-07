--
-- PG_TIMEZONE_NAMES / PG_TIMEZONE_ABBREVS
--
-- Both views report a zone's offset and DST flag AT THE CURRENT INSTANT, so
-- nothing here may name a zone that observes DST: America/New_York would print
-- -05/f in January and -04/t in July. The Etc/* zones are frozen by definition,
-- and the rest of this file checks bulk properties rather than contents.
--
-- The rows below were verified byte-for-byte against PostgreSQL 18.4.
--
SELECT name, abbrev, utc_offset, is_dst FROM pg_timezone_names WHERE name = 'UTC';
SELECT name, abbrev, utc_offset, is_dst
  FROM pg_timezone_names
 WHERE name IN ('Etc/GMT+5', 'Etc/GMT-8')
 ORDER BY name COLLATE "C";
SELECT pg_typeof(utc_offset) AS offset_type, pg_typeof(is_dst) AS dst_type
  FROM pg_timezone_names WHERE name = 'UTC';
-- bulk properties, which survive a tz-database update where a row listing
-- would not. The second is upstream's own sysviews check.
SELECT count(*) > 300 AS enough_zones,
       count(DISTINCT utc_offset) >= 24 AS enough_offsets
  FROM pg_timezone_names;
-- pg_timezone_abbrevs is small enough to dump in full: unlike the zone list it
-- is crabgresql's own curated table, not the tz database's.
--
-- MSK and VET are the two entries that resolve through a reference zone rather
-- than a constant offset (PostgreSQL's DYNTZ), so their rows track Moscow's and
-- Caracas's current offsets — a future change there breaks this file, as it
-- would break PostgreSQL's own output.
SELECT abbrev, utc_offset, is_dst FROM pg_timezone_abbrevs ORDER BY abbrev COLLATE "C";
-- The divergence, pinned deliberately rather than left to be discovered:
-- PostgreSQL 18.4 loads 198 abbreviations from src/timezone/tznames/Default
-- spanning 40 offsets, and crabgresql curates 15 spanning 9. The abbreviation
-- table is the datetime *parser's* accept-list, so growing it is a change to
-- value parsing rather than to this view; see crabgresql_types::tz.
--
-- The consequence for upstream: sysviews.sql's `count(distinct utc_offset) >= 24`
-- reports false here, so that test must not enter upstream_must_pass.txt.
SELECT count(*) AS abbrevs, count(DISTINCT utc_offset) AS offsets
  FROM pg_timezone_abbrevs;
-- every abbreviation this view lists is one a datetime literal accepts, which
-- is what makes the curated list the honest answer rather than an arbitrary one
SELECT timestamptz '2020-06-01 12:00:00 PDT', timestamptz '2020-06-01 12:00:00 EST';
-- both views resolve by name through the fixed OID table
SELECT 'pg_timezone_names'::regclass, 'pg_timezone_abbrevs'::regclass;
