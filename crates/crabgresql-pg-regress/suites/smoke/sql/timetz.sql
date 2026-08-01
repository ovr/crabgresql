--
-- TIMETZ (time with time zone)
-- input with numeric UTC offsets, abbreviations and named zones, HH:MM:SS+TZ
-- output, UTC-instant ordering, casts, interval arithmetic, the field
-- functions, and the zone-rotating operators. Only fixed offsets and a dated
-- named zone are used, so nothing depends on today's date. Output hand-checked
-- against PostgreSQL.
--
SELECT timetz '13:30:25.575401-04' AS neg, timetz '12:00:00+05:30' AS half, timetz '00:00+00' AS utc;
SELECT timetz '24:00:00-07' AS end_of_day, timetz '23:59:60 -07' AS leap_carry;
SELECT timetz '13:30:25.575401-04';

-- ordering is by the UTC instant (05:06:07-07 == 12:06:07 UTC)
SELECT timetz '05:06:07-07' > timetz '05:06:07+00' AS gt_utc,
       timetz '12:00:00+00' = timetz '12:00:00+00' AS eq;

-- casts, including timetz <-> time (dropping / attaching the zone)
SELECT '13:30:00-04'::timetz AS from_text;
SELECT (timetz '13:30:00-04')::text AS to_text;
SELECT (timetz '13:30:00-04')::time AS to_time;
SELECT (time '13:30:00')::timetz AS from_time;

-- a timetz column: insert, order by the UTC instant
CREATE TABLE timetz_tbl (id int4, t timetz);
INSERT INTO timetz_tbl VALUES (1, '01:00-07'), (2, '08:00-04'), (3, '12:00+00');
SELECT id, t FROM timetz_tbl ORDER BY 2;

-- interval arithmetic keeps the zone
SELECT timetz '10:00:00-04' + interval '1 hour 30 minutes' AS plus,
       timetz '10:00:00-04' - interval '2 hours' AS minus;

-- extract (numeric): local time-of-day fields plus the zone fields
SELECT extract(hour from timetz '13:30:25.575401-04') AS hour,
       extract(timezone from timetz '13:30:25.575401-04:30') AS tz,
       extract(timezone_hour from timetz '13:30:25.575401-04:30') AS tzh,
       extract(timezone_minute from timetz '13:30:25.575401-04:30') AS tzm;
SELECT extract(epoch from timetz '13:30:25.575401-04') AS epoch;
SELECT date_part('hour', timetz '13:30:25.575401-04') AS hour;

-- errors: field out of range (22008), unsupported unit (22023); recovery works
SELECT timetz '24:00:00.01-07';
SELECT extract(day from timetz '13:30:00-04');
SELECT 'still alive' AS status;

--
-- zone tokens: a fixed abbreviation resolves on its own; a named zone needs the
-- date, and gets that date's DST state
--
SELECT timetz '00:01 PDT' AS pdt, timetz '07:07 PST' AS pst, timetz '08:08 EDT' AS edt;
SELECT timetz '2003-03-07 15:36:39 America/New_York' AS winter,
       timetz '2003-07-07 15:36:39 America/New_York' AS summer;
-- the abbreviation prefix of a POSIX zone is ignored, and its offset counts west
SELECT timetz '12:00:00 UTC+10' AS posix_west, timetz '12:00 Z' AS zulu;

-- errors: a named zone with no date, and an unrecognized token, are both 22007;
-- an over-large numeric displacement is 22009
SELECT timetz '15:36:39 America/New_York';
SELECT timetz '15:36:39 m2';
SELECT timetz '15:36:39 MSK m2';
SELECT timetz '12:00:00+16:00';

--
-- AT TIME ZONE / AT LOCAL: the same instant of day read in another zone. The
-- result stays timetz, and wraps modulo a day.
--
SET TimeZone TO 'UTC';
SELECT timetz '00:01-07' AT TIME ZONE 'UTC' AS at_named,
       timetz '00:01-07' AT TIME ZONE INTERVAL '-10:00' AS at_interval,
       timetz '00:01-07' AT LOCAL AS at_local,
       timezone(timetz '00:01-07') AS func_form;
-- a fixed session zone, not a named one: `timetz` carries no date, so a named
-- zone would be read at *today's* DST state and this test would drift
SET TimeZone TO '-05:30';
SELECT timetz '00:01-07' AT LOCAL AS at_local_fixed;
-- AT LOCAL leaves a value that already carries the session's own offset alone.
-- It reads the session zone through the same accessor `timetz_in` does, so a
-- zone-less literal is unchanged rather than rotated by the difference between
-- two ways of asking the same zone for its offset.
SELECT timetz '12:00' AS lit,
       timetz '12:00' AT LOCAL AS at_local,
       timetz '12:00' = timetz '12:00' AT LOCAL AS at_local_is_identity,
       ('12:00'::time)::timetz = timetz '12:00' AT LOCAL AS cast_agrees;
RESET TimeZone;
-- an interval zone must be a fixed displacement (22023)
SELECT timetz '00:01-07' AT TIME ZONE INTERVAL '1 month';
SELECT 'still alive' AS status;
