--
-- NOW (the transaction clock)
-- the clock functions, the CURRENT_TIMESTAMP keyword family, and the
-- current-relative input specials. Every answer here is a boolean, a
-- difference or a fixed value: the expected output is golden text, so a real
-- timestamp in it would go stale one microsecond later. Output hand-checked
-- against PostgreSQL.
--
SET TimeZone = 'UTC';

-- now() is the transaction timestamp; the statement's is at or after it, and
-- the wall clock at or after that.
BEGIN;
SELECT now() = transaction_timestamp() AS same_thing,
       now() <= statement_timestamp() AS xact_first,
       statement_timestamp() <= clock_timestamp() AS stmt_before_wall;

-- and it stays put across the block, unlike the wall clock
SELECT now() = transaction_timestamp() AS still_the_same;

-- 'now' is the transaction timestamp, to the microsecond
SELECT 'now'::timestamptz = now() AS now_is_xact,
       'now'::timestamp = now()::timestamp AS now_is_local_wall_clock;

-- the keyword forms are exactly casts of now()
SELECT current_timestamp = now() AS cts,
       localtimestamp = now()::timestamp AS lts,
       current_date = now()::date AS cd,
       current_time = now()::timetz AS ct,
       localtime = now()::time AS lt;

-- a precision modifier rounds to that many fractional digits, so the result is
-- within half a second of the unrounded one either way. (Compared as
-- `timestamp`, since `timestamptz` has no subtraction operator yet.)
SELECT localtimestamp(0) - localtimestamp <= interval '500 milliseconds'
   AND localtimestamp - localtimestamp(0) <= interval '500 milliseconds'
       AS rounds_to_seconds;

-- (`current_timestamp(7)` clamps to 6 here but also raises a WARNING in
-- PostgreSQL, so it is pinned by an e2e test rather than by golden text.)
COMMIT;

-- under autocommit the transaction is the statement
SELECT now() = statement_timestamp() AS autocommit_coincides;

-- today/tomorrow/yesterday are date field tokens, one day apart
SELECT 'tomorrow'::date - 'today'::date AS ahead,
       'today'::date - 'yesterday'::date AS behind;
SELECT 'today'::date = current_date AS today_is_today;

-- being fields, they combine with a time, in either order
SELECT 'today 10:00'::timestamp - 'today'::timestamp AS ten_hours;
SELECT '10:00 today'::timestamp = 'today 10:00'::timestamp AS order_free;
SELECT 'yesterday 23:59:59.5'::timestamp - 'yesterday'::timestamp AS almost_a_day;

-- 'now' is not a field: it takes no company
SELECT 'now 10:00'::timestamp;
SELECT 'now EST'::timestamptz;

-- ...and a relative token conflicts with any other field that fixes the date,
-- a month name included
SELECT 'today today'::timestamp;
SELECT '2020-01-01 today'::timestamp;
SELECT 'Feb today'::timestamp;
SELECT 'today 5'::timestamp;

-- the reserved words that are whole values everywhere are errors in company
SELECT '2020-01-01 epoch'::timestamp;
SELECT 'now 10:00'::time;
SELECT 'today 10:00'::time;
SELECT '10:00 allballs'::timetz;

-- a relative literal resolves through every route to the type, not just a bare
-- one: an explicit text cast and an array element reach the same input function
SELECT 'now'::text::timestamp = now()::timestamp AS via_text_cast,
       ('{now}'::timestamp[])[1] = now()::timestamp AS via_array;
SELECT pg_input_is_valid('{now}', 'timestamp[]') AS array_soft_input;

-- allballs is midnight, and always at +00 — unlike 'now', which takes the
-- session zone's offset
SELECT 'allballs'::time AS t, 'allballs'::timetz AS ttz;
SET TimeZone = 'America/New_York';
SELECT 'allballs'::timetz AS still_utc;
SELECT 'today'::date = now()::date AS today_follows_the_session_zone;
SET TimeZone = 'UTC';

-- time and timetz take only those two specials; the date-shaped ones have no
-- time of day to name
SELECT 'today'::time;
SELECT 'epoch'::time;
SELECT 'infinity'::time;
SELECT 'today'::timetz;

-- ...and a date takes no time-only special
SELECT 'allballs'::date;

-- case and surrounding space are insignificant
SELECT '  ToDaY  '::date = current_date AS lenient;

-- the keyword forms are grammar, so their modifier is a bare integer literal
SELECT current_date(0);
SELECT current_timestamp(-1);
SELECT current_timestamp(1+1);
SELECT current_time(3::int);
