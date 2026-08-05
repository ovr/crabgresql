--
-- TIMESTAMPTZ
-- timestamp with time zone: input parsing (offsets, named zones, abbreviations),
-- output, comparisons, casts, the field functions, make_timestamptz, and
-- AT TIME ZONE. Most of the file pins the session zone to UTC so the output is
-- deterministic; the final section exercises a real display zone. Output
-- hand-checked against PostgreSQL's aligned format.
--
-- Render everything in UTC so the output is deterministic.
SET timezone = 'UTC';

-- output formatting: ISO datestyle with a +00 offset, fractional seconds trimmed
SELECT timestamptz '2001-02-16 20:38:40+00' AS plain;
SELECT timestamptz '2001-02-16 20:38:40.5+00' AS half,
       timestamptz '2001-02-16 20:38:40.999999+00' AS micros,
       timestamptz '2001-02-16 20:38:40.000001+00' AS tiny;

-- a trailing offset is converted to UTC; several spellings of -08:00
SELECT timestamptz '1997-02-10 17:32:01-08' AS off_hh,
       timestamptz '1997-02-10 17:32:01-0800' AS off_hhmm,
       timestamptz '1997-02-10 17:32:01 -08:00' AS off_colon;
-- ISO 8601 'T' with an attached offset or Z
SELECT timestamptz '2001-09-22T18:19:20-07:00' AS iso_off,
       timestamptz '2001-09-22T18:19:20Z' AS iso_zulu;
-- named zones and abbreviations resolve (DST-aware); rendered back in UTC
SELECT timestamptz '1997-02-10 17:32:01 America/New_York' AS est,
       timestamptz '1997-07-10 17:32:01 America/New_York' AS edt,
       timestamptz '1997-02-10 17:32:01 PST' AS pst,
       timestamptz '1997-02-10 17:32:01 UTC' AS utc;
-- a date with no time defaults to midnight UTC
SELECT timestamptz '1997-01-02' AS date_only;

-- special values and a BC year
SELECT timestamptz 'infinity' AS pos, timestamptz '-infinity' AS neg, timestamptz 'epoch' AS unix;
SELECT timestamptz '0097-02-16 20:00:00+00 BC' AS bc;
SELECT timestamptz 'infinity' = timestamptz '+infinity' AS inf_eq;
-- the default column name of a typed literal is the type name
SELECT timestamptz '2001-02-16 20:38:40+00';

-- comparisons drive WHERE and ORDER BY; offsets are normalized before comparing
SELECT timestamptz '2001-01-01 00:00:00-08' = timestamptz '2001-01-01 08:00:00+00' AS eq,
       timestamptz '2001-01-01+00' < timestamptz '2002-01-01+00' AS lt,
       timestamptz '-infinity' < timestamptz '2000-01-01+00' AS neg_inf_first;

-- casts both directions; timestamp<->timestamptz reinterpret in UTC
SELECT '2001-02-16 20:38:40-05'::timestamptz AS from_text;
SELECT (timestamptz '2001-02-16 20:38:40+00')::text AS to_text;
SELECT (timestamp '2001-02-16 20:38:40')::timestamptz AS ts_to_tstz;
SELECT (timestamptz '2001-02-16 20:38:40+00')::timestamp AS tstz_to_ts;

-- a timestamptz column: insert, filter, order
CREATE TABLE tstz_tbl (id int4, d1 timestamptz);
INSERT INTO tstz_tbl VALUES (1, '1997-01-02 03:04:05-08'), (2, '2001-09-22 18:19:20+00'),
                            (3, '1997-02-10 17:32:01 America/New_York');
SELECT id, d1 FROM tstz_tbl WHERE d1 < timestamptz '2000-01-01+00' ORDER BY 2;
SELECT id FROM tstz_tbl WHERE d1 = timestamptz '2001-09-22 18:19:20+00';

-- date_part (float8): calendar fields read from the UTC wall clock
SELECT date_part('year', timestamptz '2001-02-16 20:38:40+00') AS year,
       date_part('hour', timestamptz '2001-02-16 20:38:40+00') AS hour,
       date_part('epoch', timestamptz '2001-02-16 20:38:40+00') AS epoch;
-- the timezone group is 0 under the UTC display zone
SELECT date_part('timezone', timestamptz '2001-02-16 20:38:40+00') AS tz,
       date_part('timezone_hour', timestamptz '2001-02-16 20:38:40+00') AS tzh,
       date_part('timezone_minute', timestamptz '2001-02-16 20:38:40+00') AS tzm;

-- extract (numeric) and the default column name "extract"
SELECT extract(second from timestamptz '2001-02-16 20:38:40.5+00') AS second,
       extract(epoch from timestamptz '2001-02-16 20:38:40.5+00') AS epoch;
SELECT extract(year from timestamptz '2001-02-16 20:38:40+00');

-- date_trunc returns timestamptz (still +00)
SELECT date_trunc('hour', timestamptz '2001-02-16 20:38:40.5+00') AS hour,
       date_trunc('day', timestamptz '2001-02-16 20:38:40.5+00') AS day;
SELECT date_trunc('day', timestamptz 'infinity') AS inf;

-- fields of an infinite value: monotonic fields are +/-Infinity, oscillating NULL
SELECT date_part('year', timestamptz 'infinity') AS year,
       extract(epoch from timestamptz '-infinity') AS neg_epoch,
       extract(day from timestamptz 'infinity') AS day;

-- isfinite and make_timestamptz (6-arg is UTC; 7-arg applies the zone)
SELECT isfinite(timestamptz '2001-02-16+00') AS finite, isfinite(timestamptz 'infinity') AS not_finite;
SELECT make_timestamptz(2013, 7, 15, 8, 15, 23.5) AS made_utc,
       make_timestamptz(2013, 7, 15, 17, 15, 23, 'America/New_York') AS made_edt;

-- AT TIME ZONE both directions, and the timezone() function form
SELECT timestamp '2001-02-16 20:38:40' AT TIME ZONE 'America/New_York' AS ts_at_zone;
SELECT timestamptz '2001-02-16 20:38:40+00' AT TIME ZONE 'America/New_York' AS tstz_at_zone;
SELECT timezone('America/New_York', timestamp '2001-02-16 20:38:40') AS fn_to_tstz,
       timezone('America/New_York', timestamptz '2001-02-16 20:38:40+00') AS fn_to_ts;
-- a DST-varying zone across the 2011 Moscow spring-forward gap (+3 -> +4)
SELECT timestamptz '2011-03-27 01:00:00 Europe/Moscow' AS msk_before_gap,
       timestamptz '2011-03-27 03:00:00 Europe/Moscow' AS msk_after_gap;

-- the non-throwing input API
SELECT pg_input_is_valid('2001-02-16 20:38:40+00', 'timestamptz') AS ok,
       pg_input_is_valid('garbage', 'timestamptz') AS bad,
       pg_input_is_valid('2001-01-01 00:00 Nowhere/Nozone', 'timestamptz') AS bad_zone;

-- cross-type timestamp/timestamptz comparison and assignment resolve via the
-- implicit cast (identity under the UTC display zone), as in PG
SELECT timestamptz '2000-01-01 08:00:00+00' = timestamp '2000-01-01 08:00:00' AS tstz_eq_ts,
       timestamp '2000-01-01 00:00:00' < timestamptz '2000-06-01 00:00:00+00' AS ts_lt_tstz;
CREATE TABLE tstz_asgn (d1 timestamptz);
INSERT INTO tstz_asgn VALUES (timestamp '2002-05-05 05:05:05');
SELECT d1 FROM tstz_asgn;

-- out-of-range: a year past the range, an offset/constructor pushing past the
-- boundary — all report 22008 rather than overflowing
SELECT timestamptz '300000-01-01';
SELECT make_timestamptz(294276, 12, 31, 23, 0, 0, '-10');
SELECT timestamp '294276-12-31 23:59:59' AT TIME ZONE 'America/New_York';

-- errors: unparseable input 22007, unknown unit 22023, unknown zone; a bogus
-- glued zone is a syntax error (not a silently-ignored zone); recovery works
SELECT timestamptz 'garbage';
SELECT timestamptz '2001-02-16+garbage';
SELECT date_part('bogus', timestamptz '2001-02-16+00');
SELECT make_timestamptz(2013, 7, 15, 8, 15, 23, 'Nowhere/Nozone');
SELECT 'still alive' AS status;

--
-- the session display zone (SET TimeZone). Everything below is pinned against
-- PostgreSQL 18.4.
--
SET TimeZone = 'America/New_York';
SHOW TimeZone;
-- a zone-less literal is read in the session zone; an explicit token is not
SELECT timestamptz '2024-06-01 12:00:00' AS summer;
SELECT timestamptz '2024-01-15 12:00:00' AS winter;
SELECT timestamptz '2024-06-01 12:00:00+00' AS explicit_token;
-- the conversions that are an identity only under UTC
SELECT timestamp '2024-06-01 12:00:00'::timestamptz AS ts_to_tstz;
SELECT timestamptz '2024-06-01 12:00:00+00'::timestamp AS tstz_to_ts;
SELECT date '2024-06-01'::timestamptz AS date_to_tstz;
SELECT timestamptz '2024-06-01 02:00:00+00'::date AS tstz_to_date;
SELECT make_timestamptz(2024, 6, 1, 12, 0, 0) AS make_without_zone;
-- date_trunc re-resolves the offset, so it lands on local midnight across DST
SELECT date_trunc('day', timestamptz '2024-03-10 15:00:00-04') AS spring_forward;
-- offset fields report the zone; ordinary fields read the local clock; epoch does not
SELECT date_part('timezone', timestamptz '2024-01-15 12:00:00-05') AS tz,
       date_part('timezone_hour', timestamptz '2024-01-15 12:00:00-05') AS tz_hour;
SELECT date_part('day', timestamptz '2024-01-01 02:00:00+00') AS local_day,
       date_part('epoch', timestamptz '2024-01-01 02:00:00+00') AS epoch;
-- to_char's TZ/OF report the session zone
SELECT to_char(timestamptz '2024-01-15 12:00:00-05', 'HH24:MI TZ OF') AS tochar;
-- a sub-hour zone widens the printed offset
SET TimeZone = 'Asia/Kolkata';
SELECT timestamptz '2024-06-01 12:00:00' AS kolkata;
SELECT to_char(timestamptz '2024-01-15 12:00:00+05:30', 'TZ OF') AS tochar_kolkata;
-- a bare numeric GUC value is POSIX-signed: '+05:30' means UTC-5:30
SET TimeZone = '+05:30';
SHOW TimeZone;
SELECT timestamptz '2024-06-01 12:00:00+00' AS posix_signed;
-- the numeric statement forms count east instead, and show PG's POSIX spec
SET TIME ZONE 7;
SHOW TimeZone;
SELECT timestamptz '2024-06-01 12:00:00+00' AS east_seven;
-- back to UTC explicitly: `RESET` restores the *boot* value, which is UTC here
-- and the host zone in stock PostgreSQL, so it is not comparable across the two.
SET TimeZone = 'UTC';
SHOW TimeZone;
SELECT timestamptz '2024-06-01 12:00:00' AS back_to_utc;
-- date_trunc: `day` and coarser re-resolve the offset (landing on local
-- midnight across DST), while `hour` and finer keep the input's — which matters
-- inside the fall-back fold, where the truncated clock is ambiguous.
SET TimeZone = 'America/New_York';
SELECT date_trunc('hour', timestamptz '2024-11-03 01:30:00-04') AS fold_hour;
SELECT date_trunc('minute', timestamptz '2024-11-03 01:30:00-04') AS fold_minute;
SELECT date_trunc('day', timestamptz '2024-11-03 01:30:00-04') AS fold_day;
SELECT date_trunc('day', timestamptz '2024-03-10 15:00:00-04') AS spring_forward_day;
-- to_char's OF never widens to seconds, where timestamptz output does
SELECT to_char(timestamptz '1875-06-01 12:00:00', 'OF') AS of_lmt;
SELECT timestamptz '1875-06-01 12:00:00' AS out_lmt;
-- TZH/TZM split the same offset OF renders: the sign rides on the hours, the
-- minutes are the bare magnitude, and neither widens to seconds
SELECT to_char(timestamptz '2024-06-15 03:30:45+00', 'TZH:TZM|OF|TZ|tz') AS tzh_tzm;
SELECT to_char(timestamptz '1875-06-01 12:00:00', 'TZH:TZM') AS tzh_lmt;
-- casts down to the time-of-day types: a timestamptz is rotated into the
-- session zone first, so a 03:30 UTC instant is the previous evening here, and
-- timetz keeps the offset in effect at that instant
SELECT timestamptz '2024-06-15 03:30:45.5+00'::time AS tstz_time,
       timestamptz '2024-06-15 03:30:45.5+00'::timetz AS tstz_timetz,
       timestamp '2024-06-15 03:30:45.5'::time AS ts_time;
SELECT timestamptz '2024-01-15 03:30:45+00'::timetz AS winter_timetz;
-- an infinite input casts to NULL rather than erroring
SELECT timestamptz 'infinity'::time IS NULL AS inf_time_is_null,
       timestamp '-infinity'::time IS NULL AS neg_inf_time_is_null;
SET TimeZone = 'UTC';


-- date_bin on a timestamptz bins the UTC instant, so unlike date_trunc it never
-- consults the session zone; the results below are identical under any TimeZone
SELECT date_bin(interval '15 days', timestamptz '2020-02-11 15:44:17.71393+00', timestamptz '2001-01-01+00') AS d15,
       date_bin(interval '2 hours', timestamptz '2020-02-11 15:44:17.71393+00', timestamptz '2001-01-01+00') AS h2,
       date_bin(interval '250 microseconds', timestamptz '2020-02-11 15:44:17.71393+00', timestamptz '2001-01-01+00') AS us250;
SET TimeZone = 'Asia/Kolkata';
SELECT date_bin(interval '2 hours', timestamptz '2020-02-11 15:44:17.71393+00', timestamptz '2001-01-01+00') AT TIME ZONE 'UTC' AS same_instant;
SET TimeZone = 'UTC';
SELECT date_bin(interval '5 min', timestamptz '2020-02-01 01:01:01+00', timestamptz '2020-02-01 00:02:30+00') AS shifted;
SELECT date_bin(interval '30 minutes', timestamptz '2024-02-01 15:00:00+00', timestamptz '2024-02-01 17:00:00+00') AS below;
SELECT date_bin(interval '1 hour', timestamptz 'infinity', timestamptz '2001-01-01+00') AS inf;
SELECT date_bin(interval '5 years', timestamptz '2020-02-01 01:01:01+00', timestamptz '2001-01-01+00');
SELECT date_bin(interval '0 days', timestamptz '2020-02-01 01:01:01+00', timestamptz '2001-01-01+00');
SELECT date_bin(interval '200000000 days', timestamptz '2024-02-01+00', timestamptz '2024-01-01+00');
-- overload resolution: untyped literals reach the preferred type (timestamptz),
-- and so do two `date` arguments; only a genuine timestamp pair picks the
-- timestamp overload
SELECT pg_typeof(date_bin('1 h', '2020-01-05 05:30', '2001-01-01')) AS unknown_args,
       pg_typeof(date_bin(interval '1 h', date '2020-01-05', date '2001-01-01')) AS date_args,
       pg_typeof(date_bin(interval '1 h', timestamp '2020-01-05 05:30', timestamp '2001-01-01'))
         AS ts_args;
SELECT 'still alive' AS status;


--
-- date_trunc(text, timestamptz, text): truncation in an explicit zone, which
-- overrides the session's. Only timestamptz has this form.
--
SET TimeZone = 'America/New_York';
-- local midnight in UTC is the previous evening here
SELECT date_trunc('day', timestamptz '2024-03-10 15:00:00-04', 'UTC') AS day_in_utc;
SELECT date_trunc('day', timestamptz '2001-02-16 20:38:40+00', 'Australia/Sydney') AS sydney;
-- a fixed-offset abbreviation, and a variable-offset one (VET ran at -04:30
-- from 2007 to 2016 and at -04 either side of it)
SELECT date_trunc('day', timestamptz '2001-02-16 20:38:40+00', 'GMT') AS gmt,
       date_trunc('day', timestamptz '2001-02-16 20:38:40+00', 'VET') AS vet_2001,
       date_trunc('day', timestamptz '2010-06-16 20:38:40+00', 'VET') AS vet_2010;
-- the sub-day units keep the input's offset in the named zone too; `day` and
-- coarser re-resolve it, which is what lands them on real local midnight
SELECT date_trunc('hour', timestamptz '2024-11-03 01:30:00-04', 'UTC') AS fold_hour_in_utc,
       date_trunc('day', timestamptz '2024-11-03 01:30:00-04', 'Australia/Sydney') AS fold_day_in_sydney;
-- Santiago springs forward *at* midnight on 2026-09-06, so the truncated wall
-- clock does not exist; the pre-transition offset wins
SELECT date_trunc('day', timestamptz '2026-09-06 12:00:00+00', 'America/Santiago') AS gap_midnight;
-- the infinities pass through, and NULL anywhere gives NULL
SELECT date_trunc('week', timestamptz 'infinity', 'GMT') AS inf_zone_trunc;
SELECT date_trunc('day', NULL::timestamptz, 'UTC') IS NULL AS null_value,
       date_trunc('day', timestamptz '2001-02-16 20:38:40+00', NULL) IS NULL AS null_zone;
-- the zone is resolved before the unit is looked at. The unit message below is
-- PG 14's wording, this suite's baseline; PG 16+ rephrased it as
-- `unit "bogus" not recognized for type timestamp with time zone`
SELECT date_trunc('bogus', timestamptz '2001-02-16 20:38:40+00', 'Nowhere/Nozone');
SELECT date_trunc('bogus', timestamptz '2001-02-16 20:38:40+00', 'UTC');
-- a timestamp argument reaches this overload through the implicit cast. (There
-- is no interval form; the `42883` it raises is omitted here because our
-- "function does not exist" error still lacks PG's LINE/caret and HINT.)
SELECT date_trunc('day', timestamp '2001-02-16 20:38:40', 'UTC') AS ts_arg;
SET TimeZone = 'UTC';
SELECT 'still alive' AS status;


--
-- AT TIME ZONE reads a bare numeric offset the POSIX way (hours *west*), which
-- is the reverse of the same spelling inside a value. The colon-less ±HHMM form
-- is value-only, and the accepted magnitude runs far wider than the ±15:59:59 a
-- value allows.
--
SELECT timestamp '2001-06-16 12:00:00' AT TIME ZONE '+05:30' AS posix_west,
       timestamptz '2001-06-16 12:00:00+05:30' AS iso_east_in_value;
SELECT timestamp '2001-06-16 12:00:00' AT TIME ZONE '05:30' AS unsigned_is_west,
       timestamp '2001-06-16 12:00:00' AT TIME ZONE '-05:30' AS minus_is_east;
SELECT timestamp '2001-06-16 12:00:00' AT TIME ZONE '+16' AS wide_band,
       timestamp '2001-06-16 12:00:00' AT TIME ZONE 'UTC+10' AS abbrev_prefix;
SELECT timestamp '2001-06-16 12:00:00' AT TIME ZONE '+0530';
SELECT timestamp '2001-06-16 12:00:00' AT TIME ZONE '+168';
-- a zone argument is matched as given: a padded *name* is not that zone, while
-- a displacement may carry leading whitespace
SELECT timestamp '2001-06-16 12:00:00' AT TIME ZONE ' +05:30' AS padded_offset;
SELECT timestamp '2001-06-16 12:00:00' AT TIME ZONE 'UTC ';
SELECT timestamp '2001-06-16 12:00:00' AT TIME ZONE ' America/New_York';
-- a token that is neither a name nor a plain displacement is a POSIX TZ spec,
-- so one carrying its own transition rule observes DST, and the quoted form
-- resolves -- the latter is how `SET TIME ZONE 7` names itself
SELECT timestamp '2001-06-16 12:00:00' AT TIME ZONE 'EST5EDT,M3.2.0/2,M11.1.0/2' AS spec_june,
       timestamp '2001-01-16 12:00:00' AT TIME ZONE 'EST5EDT,M3.2.0/2,M11.1.0/2' AS spec_jan,
       timestamp '2001-06-16 12:00:00' AT TIME ZONE '<+07>-07' AS quoted_spec;
SELECT 'still alive' AS status;


--
-- Arithmetic. `timestamptz - timestamptz` measures the *instants* and so needs
-- no zone; `timestamptz ± interval` is a calendar operation on the *local wall
-- clock*, and the two disagree across a DST transition by exactly the hour it
-- adds or removes.
--
SET TimeZone = 'UTC';
SELECT timestamptz '2001-02-16 20:38:40+00' - timestamptz '2001-01-01 00:00:00+00' AS diff,
       timestamptz '2001-01-01 00:00:00+00' - timestamptz '2001-02-16 20:38:40+00' AS neg_diff;
-- the difference is justified to 24-hour days, as `timestamp - timestamp` is
SELECT timestamptz '2001-01-02 03:00:00+00' - timestamptz '2001-01-01 00:00:00+00' AS justified;
SELECT timestamptz '2001-01-01 00:00:00+00' + interval '1 mon 2 days 3 hours' AS plus,
       timestamptz '2001-01-01 00:00:00+00' - interval '1 day' AS minus,
       interval '1 day' + timestamptz '2001-01-01 00:00:00+00' AS commuted;
-- a month add clamps the day of month, here in the instant's own zone
SELECT timestamptz '2001-03-31 12:00:00+00' + interval '1 mon' AS clamp_eom;
-- `timestamptz` is the preferred datetime type, so a mixed pair widens the
-- other side rather than narrowing this one
SELECT timestamptz '2001-01-02 00:00:00+00' - timestamp '2001-01-01 00:00:00' AS minus_ts,
       timestamptz '2001-01-02 00:00:00+00' - date '2001-01-01' AS minus_date,
       date '2001-01-01' - timestamptz '2001-01-02 00:00:00+00' AS date_minus;
-- an infinity swallows a finite interval; opposite infinities conflict
SELECT timestamptz 'infinity' + interval '1 day' AS inf_plus,
       timestamptz '2001-01-01+00' + interval 'infinity' AS to_inf,
       timestamptz '2001-01-01+00' - interval 'infinity' AS to_neg_inf;
SELECT timestamptz 'infinity' - interval 'infinity';
-- operators PostgreSQL does not have
SELECT timestamptz '2001-01-01+00' + timestamptz '2001-01-01+00';
SELECT interval '1 day' - timestamptz '2001-01-01+00';

-- Under a DST zone, `1 day` keeps the wall-clock hour and `24 hours` keeps the
-- elapsed time. Spring forward makes the day 23 hours long, fall back 25.
SET TimeZone = 'America/New_York';
SELECT timestamptz '2024-03-09 12:00:00-05' + interval '1 day' AS calendar_day,
       timestamptz '2024-03-09 12:00:00-05' + interval '24 hours' AS elapsed_day;
SELECT timestamptz '2024-11-02 12:00:00-04' + interval '1 day' AS calendar_day,
       timestamptz '2024-11-02 12:00:00-04' + interval '24 hours' AS elapsed_day;
-- months and days each re-resolve the offset before the next part is applied
SELECT timestamptz '2024-03-09 12:00:00-05' + interval '1 mon 1 day 1 hour' AS stepped;
-- a shifted wall clock in the spring-forward gap moves forward past it; one in
-- the fall-back fold takes the offset after the transition
SELECT timestamptz '2024-03-09 02:30:00-05' + interval '1 day' AS gap,
       timestamptz '2024-11-02 01:30:00-04' + interval '1 day' AS fold;

--
-- age() reads the *wall clocks*, so it answers in calendar units: two noons
-- either side of a spring-forward are `2 days` apart even though only 47 hours
-- elapsed. The one-argument form measures from `current_date`.
--
SELECT age(timestamptz '2024-03-11 12:00:00-04', timestamptz '2024-03-09 12:00:00-05') AS calendar,
       timestamptz '2024-03-11 12:00:00-04' - timestamptz '2024-03-09 12:00:00-05' AS elapsed;
SET TimeZone = 'UTC';
SELECT age(timestamptz '2001-04-10 00:00:00+00', timestamptz '1957-06-13 00:00:00+00') AS age2;
-- `timestamptz` wins the overload for two dates, and for a mixed pair
SELECT pg_typeof(age(date '2001-01-01', date '2000-01-01')) AS resolved,
       age(date '2001-01-01', date '2000-01-01') AS from_dates,
       age(timestamptz '2001-01-01+00', timestamp '2000-01-01') AS mixed;
-- The one-argument forms anchor at today, so pin them by comparison. The step
-- has to be a day: `- interval '1 mon'` clamps the day of month, so on the 31st
-- of a month whose predecessor is shorter the answer is `1 mon 3 days`, and the
-- assertion would be false seven days a year.
SELECT age(current_date::timestamp) = interval '0' AS today_is_zero,
       age(current_date::timestamptz) = interval '0' AS today_tz_is_zero,
       age(current_date::timestamptz - interval '1 day') = interval '1 day' AS one_day_ago;
-- the infinity matrix is the same for both types
SELECT age(timestamptz 'infinity') AS inf, age(timestamptz '-infinity') AS neg_inf;
SELECT age(timestamp 'infinity') AS inf, age(timestamp '-infinity') AS neg_inf;
SELECT age(timestamptz 'infinity', timestamptz '-infinity') AS opposite,
       age(timestamptz '-infinity', timestamptz 'infinity') AS reversed;
SELECT age(timestamptz 'infinity', timestamptz 'infinity');
SELECT age(timestamp '-infinity', timestamp '-infinity');
SELECT 'still alive' AS status;
