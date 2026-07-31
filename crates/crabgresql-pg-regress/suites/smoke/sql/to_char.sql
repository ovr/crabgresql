--
-- TO_CHAR / TO_DATE / TO_TIMESTAMP / TO_NUMBER
-- Format-picture rendering and parsing. Covers the datetime code set
-- (YYYY/MM/DD, HH/HH12/HH24, MI/SS/MS/US, AM/PM, month and day names, TZ/OF,
-- FM, TH, quoted literals), the numeric picture codes (9/0/./,/S/MI/PL/SG/PR/
-- L/V/RN/EEEE/FM), overload resolution across the two families, and the error
-- paths. Output hand-checked against PostgreSQL's aligned format.
--
-- The codes Q, W, WW, IW, IYYY, DDD, J, SSSS, RM, CC, BC/AD, SP, TM and D are
-- deliberately not implemented and pass through as literal text, so no picture
-- below uses them (nor any unquoted letter that would be read as one).
--
-- Render everything in UTC so TZ/OF and to_timestamp are deterministic.
SET timezone = 'UTC';

-- numeric datetime codes
SELECT to_char(timestamp '2024-03-05 14:07:09.987654',
               'YYYY|YYY|YY|Y|MM|DD|HH|HH12|HH24|MI|SS|MS|US') AS codes;

-- sub-second fields truncate and are left-scaled: .9 is 900 ms
SELECT to_char(timestamp '2024-03-05 14:07:09.999999', 'HH24:MI:SS') AS truncated,
       to_char(timestamp '2024-03-05 14:07:09.9', 'MS|US') AS scaled;

-- month and day names, blank-padded to nine characters
SELECT to_char(timestamp '2024-03-05', 'Month|MONTH|month') AS full_month;
SELECT to_char(timestamp '2024-03-05', 'Mon|MON|mon') AS abbr_month;
SELECT to_char(timestamp '2024-03-05', 'Day|DAY|day') AS full_day;
SELECT to_char(timestamp '2024-03-05', 'Dy|DY|dy') AS abbr_day;

-- only the exact spellings are codes; anything else passes through
SELECT to_char(timestamp '2024-03-05', '[MOnth][moNTH][MOn][aM][Am]') AS passthrough;

-- FM strips both zero padding and blank padding
SELECT to_char(timestamp '2024-03-05', 'FMYYYY FMMM FMDD') AS fm_numeric,
       to_char(timestamp '2024-03-05', 'FMMonth|FMDay|FMMon|FMDy') AS fm_names;

-- the meridiem codes render the value, not the spelling
SELECT to_char(timestamp '2024-03-05 00:30:00',
               'HH|HH12|AM|am|A.M.|a.m.|PM|pm|P.M.|p.m.') AS midnight;
SELECT to_char(timestamp '2024-03-05 13:30:00',
               'AM|am|A.M.|a.m.|PM|pm|P.M.|p.m.') AS afternoon;

-- the TH/th ordinal suffix
SELECT to_char(timestamp '2024-03-05 14:00:00',
               'DDTH|DDth|MMth|HH24th|FMDDTH') AS ordinals;

-- BC years render their display (positive) year
SELECT to_char(timestamp '0001-01-01 BC', 'YYYY|YYY|YY|Y|MM|DD') AS bc;

-- quoted literal text, and a backslash which is not an escape
SELECT to_char(timestamp '2024-03-05 14:00:00', '\HH24 "q"" YYYY"') AS quoted;

-- zone codes: a timestamptz has an abbreviation, a plain timestamp does not
SELECT to_char(timestamptz '2024-03-05 14:00:00+00', 'TZ|tz|OF') AS with_zone;
SELECT '[' || to_char(timestamp '2024-03-05', 'TZ') || ']['
            || to_char(timestamp '2024-03-05', 'OF') || ']' AS without_zone;

-- NULL input and non-finite values yield NULL
SELECT to_char(timestamp 'infinity', 'YYYY') IS NULL AS inf_is_null,
       to_char(timestamp '2024-03-05', NULL) IS NULL AS null_fmt,
       to_char(NULL::timestamp, 'YYYY') IS NULL AS null_value;

-- date and time reach the timestamptz and interval overloads through casts
SELECT to_char(date '2024-03-05', 'YYYY-MM-DD HH24:MI:SS|TZ|OF') AS from_date;
SELECT to_char(time '14:07:09.5', 'HH24:MI:SS.MS|YYYY|MM|DD') AS from_time;

-- interval formatting, including its signed-field quirks
SELECT to_char(interval '1 year 2 mons 3 days 04:05:06',
               'YYYY-MM-DD HH24:MI:SS') AS iv,
       to_char(interval '25 hours', 'HH24|HH12') AS iv_hours,
       to_char(interval '-2 days -3 hours', 'DD HH24') AS iv_negative;

-- the calendar codes are not meaningful for an interval
SELECT to_char(interval '1 day', 'Month');
SELECT to_char(interval '1 day', 'TZ');

-- to_date / to_timestamp: defaults, separators, trailing garbage
SELECT to_date('2024', 'YYYY') AS year_only,
       to_date('2024-02', 'YYYY-MM') AS year_month,
       to_date('', 'YYYY-MM-DD') AS empty;
SELECT to_date('2024/03/05', 'YYYY-MM-DD') AS separators,
       to_date('2024-01-01xyz', 'YYYY-MM-DD') AS garbage,
       to_date('  2024   03 ', 'YYYY MM') AS spaces;
SELECT to_timestamp('2024-03-05 14:07:09', 'YYYY-MM-DD HH24:MI:SS') AS full,
       to_timestamp('2024-03-05', 'YYYY-MM-DD HH24:MI:SS') AS short;

-- short-year completion: the window comes from the value, and four spelled-out
-- digits (or a YYYY code) never complete
SELECT to_date('5', 'Y') AS y1,
       to_date('69', 'YY') AS y69,
       to_date('70', 'YY') AS y70,
       to_date('070', 'YYY') AS y070,
       to_date('520', 'YYY') AS y520,
       to_date('515', 'YYY') AS y515;
SELECT to_date('0070', 'YYYY') AS yyyy,
       to_date('0070', 'YYY') AS four_digits,
       to_date('70', 'YYYY') AS short_yyyy;

-- a field's width is capped only when another field follows it directly
SELECT to_date('20240305', 'YYYYMMDD') AS packed,
       to_date('2024', 'YY') AS greedy,
       to_date('2024-01', 'YY-MM') AS greedy_sep;

-- names, ordinals and the twelve-hour clock on input
SELECT to_date('05 MARCH 2024', 'DD MONTH YYYY') AS month_name,
       to_date('05 mar 2024', 'DD Mon YYYY') AS month_abbr,
       to_date('5th 2024', 'DDTH YYYY') AS ordinal;
SELECT to_timestamp('2024-03-05 12:00 PM', 'YYYY-MM-DD HH12:MI AM') AS noon,
       to_timestamp('2024-03-05 12:00 AM', 'YYYY-MM-DD HH12:MI AM') AS midnight;

-- MS/US are left-scaled on input too
SELECT to_timestamp('2024 3', 'YYYY MS') AS ms,
       to_timestamp('2024 12', 'YYYY US') AS us;

-- to_timestamp(float8): seconds since the Unix epoch
SELECT to_timestamp(0) AS epoch,
       to_timestamp(1700000000.123456) AS fractional,
       to_timestamp(-1) AS before_epoch;
SELECT to_timestamp('infinity'::float8) AS inf;
SELECT to_timestamp('NaN'::float8);
SELECT to_timestamp(1e18);

-- a separator must leave the offset its sign, and FM binds to the very next
-- node -- quoted text and passthrough included
SELECT to_timestamp('2024-03-05 10:00:00 -05', 'YYYY-MM-DD HH24:MI:SS OF') AS spaced_of,
       to_timestamp('2024-03-05 10:00:00-05', 'YYYY-MM-DD HH24:MI:SSOF') AS tight_of,
       to_date('2024 -03', 'YYYY MM') AS separator_eats_one;
SELECT to_char(timestamp '2024-03-05 04:00:00', 'FM"a"HH24') AS fm_then_quote,
       to_char(interval '1 day', 'TH FM YYYY th') AS fm_then_space;

-- the date range's upper bound is exclusive
SELECT to_date('5874897-12-31', 'YYYY-MM-DD') AS max_date;
SELECT to_date('5874898', 'YYYY');
SELECT '5874898-01-01'::date;

-- an oversized field is reported, not overflowed into the calendar arithmetic
SELECT to_date('100000000000000000', 'YYYY');
SELECT to_timestamp('2024 999999999999999999', 'YYYY US');
SELECT to_timestamp('2024 9999999', 'YYYY US');

-- datetime parse errors
SELECT to_date('2024-XX-05', 'YYYY-MM-DD');
SELECT to_date('garbage', 'YYYY-MM-DD');
SELECT to_timestamp('abc', 'Mon');
SELECT to_timestamp('2024-03-05 13:00 PM', 'YYYY-MM-DD HH12:MI AM');
SELECT to_timestamp('2024-03-05 25:00', 'YYYY-MM-DD HH24:MI');
SELECT to_date('2024-02-30', 'YYYY-MM-DD');

--
-- numeric pictures
--
-- overload resolution: int4, int8, numeric, float4, float8
SELECT to_char(1, '999') AS int4,
       to_char(1::int8, '999') AS int8,
       to_char(1.5, '999.9') AS numeric,
       to_char(1.5::float4, '999.9999999') AS float4,
       to_char(0.1::float8, '0.999999999999999999') AS float8;

-- an untyped first argument cannot pick a best candidate. Overload-resolution
-- errors carry no source span here, so the expected output omits PG's
-- `LINE 1:` caret (and the undefined-function HINT), as `aggregate.out` does.
SELECT to_char('2024-01-01', 'YYYY');
SELECT to_char('x'::text, 'y');

-- sign placement: the default sign and S float, MI/PL/SG/PR are anchored
SELECT '[' || to_char(1, '99') || '][' || to_char(-1, '99') || ']' AS plain,
       '[' || to_char(1, 'S99') || '][' || to_char(-1, 'S99') || ']' AS lead_s,
       '[' || to_char(1, '99S') || '][' || to_char(-1, '99S') || ']' AS trail_s;
SELECT '[' || to_char(1, 'MI99') || '][' || to_char(-1, 'MI99') || ']' AS mi,
       '[' || to_char(1, 'PL99') || '][' || to_char(-1, 'PL99') || ']' AS pl,
       '[' || to_char(1, 'SG99') || '][' || to_char(-1, 'SG99') || ']' AS sg,
       '[' || to_char(1, '99PR') || '][' || to_char(-1, '99PR') || ']' AS pr;
SELECT '[' || to_char(1, 'L99') || '][' || to_char(-1, 'L99') || ']' AS lead_l,
       '[' || to_char(1, '99L') || '][' || to_char(-1, '99L') || ']' AS trail_l;

-- zero suppression, forced zeros and blanked group separators
SELECT '[' || to_char(1, '0999') || '][' || to_char(1, '9099') || ']' AS zeros,
       '[' || to_char(1, '9,999') || '][' || to_char(1234, '9,999') || ']' AS groups;
SELECT '[' || to_char(0, '99') || '][' || to_char(0, '9999.9999') || ']' AS zero,
       '[' || to_char(0.5, '9999.99') || ']' AS half;

-- rounding is half away from zero, and a value rounding to zero loses its sign
SELECT '[' || to_char(2.5, '9') || '][' || to_char(-2.5, '9') || ']' AS halves,
       '[' || to_char(1.005, '9.99') || '][' || to_char(-0.01, '99.9') || ']' AS carry;

-- overflow fills the digit positions with #, keeping the punctuation
SELECT '[' || to_char(1234, '99') || '][' || to_char(12345, '9,999.99') || ']' AS over,
       '[' || to_char(-12345, '9,999.99PR') || '][' || to_char(1, '.9') || ']' AS over_pr;

-- FM suppresses all padding, and trims trailing 9-coded fraction zeros
SELECT '[' || to_char(1, 'FM9') || '][' || to_char(1.5, 'FM99.999') || ']' AS fm,
       '[' || to_char(1.5, 'FM99.990') || '][' || to_char(0, 'FM999.999') || ']' AS fm_zero,
       '[' || to_char(-1, 'FM99PR') || ']' AS fm_pr;

-- NaN and the infinities
SELECT '[' || to_char('NaN'::numeric, '999') || '][' || to_char('NaN'::numeric, '9999.99') || ']' AS nan,
       '[' || to_char('Infinity'::numeric, '9999.99') || '][' || to_char('-Infinity'::numeric, '9999PR') || ']' AS inf;
-- an infinity has no exponent form either, so EEEE overflows too
SELECT '[' || to_char('Infinity'::numeric, '9.99EEEE') || '][' || to_char('-Infinity'::numeric, '9.99EEEE') || ']' AS inf_sci;
-- more digit positions than an i64 holds, for the TH accumulator
SELECT '[' || to_char(99999999999999999999999::numeric, '99999999999999999999999TH') || ']' AS wide_th;

-- V shifts the value, TH suffixes it, RN spells it out
SELECT '[' || to_char(1234, '999V9') || '][' || to_char(1.2, '99V9') || ']' AS v,
       '[' || to_char(1, '999th') || '][' || to_char(2, '999TH') || '][' || to_char(-1, '999th') || ']' AS th;
SELECT '[' || to_char(4, 'RN') || ']' AS rn,
       '[' || to_char(1999, 'FMRN') || '][' || to_char(4000, 'RN') || ']' AS rn_edge;

-- EEEE
SELECT '[' || to_char(-0.0004859, '9.99EEEE') || '][' || to_char(12345, '9.99EEEE') || ']' AS sci,
       '[' || to_char(0, '9.99EEEE') || '][' || to_char(1234, '9EEEE') || ']' AS sci_edge;

-- degenerate pictures
SELECT '[' || to_char(1234, '') || '][' || to_char(1234, 'FM') || ']' AS empty,
       '[' || to_char(1234, '9 9 9 9') || '][' || to_char(1234, '"x"999') || ']' AS spaced;

-- picture syntax errors
SELECT to_char(1, 'S9S');
SELECT to_char(1, '9.9.9');
SELECT to_char(1, 'MI9PR');
SELECT to_char(1, '9EEEE9');
SELECT to_char(1, '999.9V9');
SELECT to_char(1, '9PR9');
SELECT to_char(1, '9V9EEEE');

-- to_number is permissive: it lifts digits out positionally
SELECT to_number('12,454.8-', '99G999D9S') AS signed,
       to_number('$1,234.56', 'L9G999D99') AS currency,
       to_number('<123>', '999PR') AS bracketed,
       to_number('12-', '99MI') AS trailing_minus;
SELECT to_number('  1234', '9999') AS leading_blanks,
       to_number('1 2 3', '999') AS interleaved,
       to_number('a1c', '9999') AS letters,
       to_number('1.2.3', '9.9') AS extra_point;
SELECT to_number('123', '') IS NULL AS empty_format;
SELECT to_number('abc', '9999');
SELECT to_number('-', '999');
