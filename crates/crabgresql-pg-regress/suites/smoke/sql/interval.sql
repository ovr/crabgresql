--
-- INTERVAL
-- interval: input parsing (verbose, SQL-standard, ISO-8601), postgres-style
-- output, comparisons, casts, arithmetic (interval/interval, timestamp/interval,
-- timestamp-timestamp), the justify/age/make constructors, the field functions
-- (date_part / extract / date_trunc), to_char, and infinite intervals. Output
-- hand-checked against PostgreSQL's postgres IntervalStyle.
--
-- output formatting: years/mons/days then a signed HH:MM:SS, sub-second zeros
-- trimmed, zero is 00:00:00
SELECT interval '1 year 2 mons 3 days 04:05:06' AS full;
SELECT interval '1 mon' AS one_mon, interval '2 mons' AS two_mons, interval '-1 mon' AS neg_mon;
SELECT interval '1.5 days' AS day_half, interval '2.5 hours' AS hour_half, interval '1.5 mons' AS mon_half;
SELECT interval '-1 day 2 hours' AS mixed_sign, interval '-00:00:01' AS neg_sec, interval '0' AS zero;
SELECT interval '90 minutes' AS minutes, interval '100000000 days' AS many_days, interval '24:00:00' AS full_day;
SELECT interval '2 days ago' AS ago, interval '00:00:01.234567' AS micros;
-- the default column name of a typed literal is the type name
SELECT interval '1 day';

-- input forms: SQL-standard leading/trailing fields and ISO-8601 durations
SELECT interval '1' day AS one_day, interval '1-2' year to month AS year_month;
SELECT interval '3 4:05:06' day to second AS day_time, interval 'P1Y2M3DT4H5M6S' AS iso;

-- comparisons use the canonical span (30-day months, 24-hour days)
SELECT interval '2 mons' < interval '70 days' AS lt,
       interval '1 day' = interval '24 hours' AS eq,
       interval '1 hour' > interval '59 minutes' AS gt;

-- casts both directions; a column named after the target type
SELECT '1 day 2 hours'::interval AS from_text;
SELECT (interval '1 day 2 hours')::text AS to_text;

-- an interval column: insert, filter, order
CREATE TABLE iv_tbl (id int4, span interval);
INSERT INTO iv_tbl VALUES (1, '1 day'), (2, interval '2 mons 3 days'),
                          (3, '-1 day'), (4, '01:30:00');
SELECT id, span FROM iv_tbl WHERE span < interval '1 mon' ORDER BY 2;
SELECT id FROM iv_tbl WHERE span = interval '1 day';

-- interval arithmetic: add, subtract, negate, scale by a number
SELECT interval '1 day' + interval '2 hours' AS add,
       interval '5 mons' - interval '2 mons 10 days' AS sub;
SELECT - interval '1 day 2 hours' AS neg;
SELECT interval '1 day 3 hours' * 2.5 AS mul, 3 * interval '2 mons' AS scale,
       interval '1 day 3 hours' / 2 AS div;

-- timestamp / interval arithmetic (month add clamps the day of month)
SELECT timestamp '2001-01-01 12:00:00' + interval '1 mon 3 days 4 hours' AS ts_plus;
SELECT timestamp '2001-03-31' + interval '1 mon' AS clamp_eom;
SELECT timestamp '2001-01-01' - interval '1 day' AS ts_minus;
SELECT timestamp '2001-01-01' - timestamp '1997-01-02' AS ts_diff;
SELECT timestamp '2001-09-22 18:19:20' - timestamp '2001-09-22 12:00:00' AS same_day;

-- justify_days (30-day months), justify_hours (24-hour days), justify_interval
SELECT justify_days(interval '35 days') AS jdays,
       justify_hours(interval '27 hours') AS jhours,
       justify_interval(interval '1 mon 33 days 27 hours') AS jinterval;

-- make_interval and age
SELECT make_interval(1, 2, 3, 4, 5, 6, 7.5) AS made, make_interval(0, 0, 0, 0, 0, 0, 0) AS zero;
SELECT age(timestamp '2001-04-10', timestamp '1957-06-13') AS age1,
       age(timestamp '2010-01-01', timestamp '2009-03-15') AS age2;

-- date_part (float8)
SELECT date_part('year', interval '14 months') AS year,
       date_part('month', interval '14 months') AS month,
       date_part('day', interval '1 day 02:03:04') AS day;
SELECT date_part('hour', interval '1 day 02:03:04') AS hour,
       date_part('minute', interval '1 day 02:03:04') AS minute,
       date_part('second', interval '00:00:04.5') AS second;
SELECT date_part('epoch', interval '1 year 2 mons 3 days 04:05:06') AS epoch,
       date_part('quarter', interval '14 months') AS quarter;

-- extract (numeric): sub-second scales, epoch
SELECT extract(second from interval '00:00:04.5') AS second,
       extract(milliseconds from interval '00:00:04.5') AS ms,
       extract(microseconds from interval '00:00:04.5') AS us;
SELECT extract(epoch from interval '1 day 02:03:04') AS epoch;

-- date_trunc
SELECT date_trunc('hour', interval '1 day 02:03:04.55') AS hour,
       date_trunc('day', interval '1 mon 2 days 3 hours') AS day;

-- isfinite and to_char
SELECT isfinite(interval '1 day') AS finite, isfinite(interval 'infinity') AS not_finite;
SELECT to_char(interval '1 year 2 mons 3 days 04:05:06', 'YYYY-MM-DD HH24:MI:SS') AS formatted;
SELECT to_char(interval '1 day 02:03:04.567', 'FMHH24:FMMI:FMSS.MS.US') AS fm;

-- infinite intervals: parse, output, arithmetic, comparison, fields
SELECT interval 'infinity' AS pos, interval '-infinity' AS neg, interval '+infinity' AS plus;
SELECT interval 'infinity' + interval '1 day' AS still_inf, - interval 'infinity' AS negated;
SELECT interval '-infinity' < interval '1 day' AS lt, interval 'infinity' > interval '1 day' AS gt;
SELECT date_part('epoch', interval 'infinity') AS epoch, date_part('month', interval 'infinity') AS oscillating;
SELECT timestamp '2020-01-01' + interval 'infinity' AS ts_inf;

-- type modifier: the admitted fields and the precision pack into one atttypmod,
-- which format_type decodes back to the spelling that was written
CREATE TABLE interval_typmod_tbl(
  a interval, b interval(3), c interval year, d interval month, e interval day,
  f interval hour, g interval minute, h interval second, i interval second(2),
  j interval year to month, k interval day to hour, l interval day to minute,
  m interval day to second, n interval day to second(4), o interval hour to minute,
  p interval hour to second, q interval hour to second(1), r interval minute to second,
  s interval minute to second(0));
SELECT attname, atttypmod, format_type(atttypid, atttypmod) AS spelling
  FROM pg_attribute WHERE attrelid = 'interval_typmod_tbl'::regclass AND attnum > 0
  ORDER BY attnum;
-- information_schema names the fields separately from the precision
SELECT column_name, interval_type, datetime_precision, interval_precision
  FROM information_schema.columns WHERE table_name = 'interval_typmod_tbl'
  ORDER BY ordinal_position;

-- casting applies the modifier: the lowest admitted field decides what survives,
-- the fields above it are untouched, and a range reaching SECOND rounds
SELECT (interval '1 year 2 months 3 days 4:05:06.789')::interval year AS y,
       (interval '1 year 2 months 3 days 4:05:06.789')::interval month AS mo,
       (interval '1 year 2 months 3 days 4:05:06.789')::interval day AS d;
SELECT (interval '1 year 2 months 3 days 4:05:06.789')::interval hour AS h,
       (interval '1 year 2 months 3 days 4:05:06.789')::interval minute AS mi,
       (interval '1 year 2 months 3 days 4:05:06.789')::interval second(1) AS s1;
-- rounding is half away from zero, truncation is toward zero
SELECT (interval '0.005 sec')::interval second(2) AS pos,
       (interval '-0.005 sec')::interval second(2) AS neg,
       (interval '-1 day -2:30:00')::interval hour AS neg_trunc;
-- assignment into a column applies the same modifier
INSERT INTO interval_typmod_tbl(c, f, i)
  VALUES (interval '14 months 3 days', interval '1 day 2:30:45.6789', interval '1 day 2:30:45.6789');
SELECT c, f, i FROM interval_typmod_tbl;
DROP TABLE interval_typmod_tbl;

-- errors: unparseable input is 22007, an unknown unit is 22023; recovery works
SELECT interval 'garbage';
SELECT date_part('bogus', interval '1 day');
SELECT 'still alive' AS status;
