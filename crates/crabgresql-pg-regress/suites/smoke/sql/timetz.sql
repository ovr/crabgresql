--
-- TIMETZ (time with time zone)
-- input with numeric UTC offsets, HH:MM:SS+TZ output, UTC-instant ordering,
-- casts, interval arithmetic, and the field functions. Numeric offsets are used
-- throughout for determinism. Output hand-checked against PostgreSQL.
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
