--
-- TIME
-- time without time zone: input parsing, HH:MM:SS output, comparisons, casts,
-- interval arithmetic, and the field functions (date_part / extract /
-- make_time). Output hand-checked against PostgreSQL's aligned format.
--
-- output; a trailing time zone is accepted and ignored; 24:00:00 is allowed
SELECT time '13:30:25.575401' AS micros, time '00:00' AS midnight, time '24:00:00' AS end_of_day;
SELECT time '23:59:59.999999' AS max_frac, time '02:03 PST' AS zone_ignored;
-- rounding: a 7th fractional digit / a leap second carries to the next day
SELECT time '23:59:59.9999999' AS rounds_up, time '23:59:60' AS leap_carry;
SELECT time '13:30:25.575401';

-- comparisons
SELECT time '01:00' < time '02:00' AS lt, time '12:00' = time '12:00:00' AS eq;

-- casts, including time <-> interval
SELECT '13:30:00'::time AS from_text;
-- a numeric zone offset glued to the time is accepted and ignored
SELECT time '13:30:00-04' AS glued_zone;
SELECT (time '13:30:00')::text AS to_text;
SELECT (time '13:30:00')::interval AS to_interval;
SELECT (interval '13:30:00')::time AS from_interval;

-- a time column: insert, filter, order
CREATE TABLE time_tbl (id int4, t time);
INSERT INTO time_tbl VALUES (1, '00:00'), (2, '12:00'), (3, '23:59:59'), (4, '11:59:59.99 PM');
SELECT id, t FROM time_tbl WHERE t > time '05:06:07' ORDER BY 2;

-- interval arithmetic wraps within the day; time - time -> interval
SELECT time '10:00:00' + interval '1 hour 30 minutes' AS plus,
       time '10:00:00' - interval '2 hours' AS minus;
SELECT time '23:30:00' + interval '1 hour' AS wrap;
SELECT time '13:30:00' - time '08:00:00' AS diff;

-- extract (numeric) with PG's per-field scale, and date_part (float8)
SELECT extract(hour from time '13:30:25.575401') AS hour,
       extract(minute from time '13:30:25.575401') AS minute,
       extract(second from time '13:30:25.575401') AS second;
SELECT extract(microsecond from time '13:30:25.575401') AS us,
       extract(millisecond from time '13:30:25.575401') AS ms,
       extract(epoch from time '13:30:25.575401') AS epoch;
SELECT date_part('hour', time '13:30:25.575401') AS hour,
       date_part('epoch', time '13:30:25.575401') AS epoch;

-- make_time
SELECT make_time(8, 20, 0.0) AS mk;

-- errors: field out of range (22008), unsupported/unknown unit (22023);
-- recovery still works
SELECT time '25:00:00';
SELECT time '24:00:00.01';
SELECT extract(day from time '13:30:00');
SELECT extract(fortnight from time '13:30:00');
SELECT make_time(10, 55, 100.1);
SELECT 'still alive' AS status;
