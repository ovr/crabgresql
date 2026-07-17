--
-- TIMESTAMP
-- timestamp without time zone: input parsing, ISO output, comparisons, casts,
-- and the field functions (date_part / extract / date_trunc / isfinite /
-- make_timestamp). Output hand-checked against PostgreSQL's aligned format.
--
-- output formatting: ISO datestyle, fractional seconds trimmed of trailing zeros
SELECT timestamp '2001-02-16 20:38:40' AS plain;
SELECT timestamp '2001-02-16 20:38:40.5' AS half,
       timestamp '2001-02-16 20:38:40.999999' AS micros,
       timestamp '2001-02-16 20:38:40.000001' AS tiny;
-- a date with no time defaults to midnight; ISO 8601 'T' is accepted
SELECT timestamp '1997-01-02' AS date_only, timestamp '2001-09-22T18:19:20' AS iso_t;
-- the traditional verbose form; a trailing time zone is ignored by this type
SELECT timestamp 'Feb 10 17:32:01 1997' AS verbose;
SELECT timestamp '1997-06-10 17:32:01 -07:00' AS zoned;
-- special values and a BC year
SELECT timestamp 'infinity' AS pos, timestamp '-infinity' AS neg, timestamp 'epoch' AS unix;
SELECT timestamp '0097-02-16 BC' AS bc;
-- the default column name of a typed literal is the type name
SELECT timestamp '2001-02-16 20:38:40';

-- comparisons drive WHERE and ORDER BY
SELECT timestamp '2001-01-01' = timestamp '2001-01-01' AS eq,
       timestamp '2001-01-01' < timestamp '2002-01-01' AS lt,
       timestamp '-infinity' < timestamp '2000-01-01' AS neg_inf_first;

-- casts both directions; a column named after the target type
SELECT '2001-02-16 20:38:40'::timestamp AS from_text;
SELECT (timestamp '2001-02-16 20:38:40')::text AS to_text;

-- a timestamp column: insert, filter, order
CREATE TABLE ts_tbl (id int4, d1 timestamp);
INSERT INTO ts_tbl VALUES (1, '1997-01-02 03:04:05'), (2, '2001-09-22 18:19:20'),
                          (3, 'Feb 10 17:32:01 1997');
SELECT id, d1 FROM ts_tbl WHERE d1 < timestamp '2000-01-01' ORDER BY 2;
SELECT id FROM ts_tbl WHERE d1 = timestamp '2001-09-22 18:19:20';

-- date_part (float8)
SELECT date_part('year', timestamp '2001-02-16 20:38:40') AS year,
       date_part('month', timestamp '2001-02-16 20:38:40') AS month,
       date_part('day', timestamp '2001-02-16 20:38:40') AS day;
SELECT date_part('hour', timestamp '2001-02-16 20:38:40.5') AS hour,
       date_part('minute', timestamp '2001-02-16 20:38:40.5') AS minute,
       date_part('second', timestamp '2001-02-16 20:38:40.5') AS second;
SELECT date_part('dow', timestamp '2001-02-16') AS dow,
       date_part('isodow', timestamp '2001-02-16') AS isodow,
       date_part('doy', timestamp '2001-02-16') AS doy;
SELECT date_part('quarter', timestamp '2001-12-31') AS quarter,
       date_part('week', timestamp '2001-02-16') AS week,
       date_part('isoyear', timestamp '2001-02-16') AS isoyear;
SELECT date_part('decade', timestamp '2001-02-16') AS decade,
       date_part('century', timestamp '2001-02-16') AS century,
       date_part('millennium', timestamp '2001-02-16') AS millennium;
SELECT date_part('epoch', timestamp '2001-02-16 20:38:40') AS epoch,
       date_part('microseconds', timestamp '2001-02-16 20:38:40.5') AS us,
       date_part('milliseconds', timestamp '2001-02-16 20:38:40.5') AS ms;

-- extract (numeric): integer fields, and the sub-second scales PG uses
SELECT extract(year from timestamp '2001-02-16 20:38:40') AS year,
       extract(day from timestamp '2001-02-16 20:38:40') AS day;
SELECT extract(second from timestamp '2001-02-16 20:38:40.5') AS second,
       extract(milliseconds from timestamp '2001-02-16 20:38:40.5') AS ms,
       extract(microseconds from timestamp '2001-02-16 20:38:40.5') AS us;
SELECT extract(epoch from timestamp '2001-02-16 20:38:40.5') AS epoch;
-- default column name of EXTRACT is "extract"
SELECT extract(year from timestamp '2001-02-16');

-- date_trunc
SELECT date_trunc('hour', timestamp '2001-02-16 20:38:40.5') AS hour,
       date_trunc('day', timestamp '2001-02-16 20:38:40.5') AS day;
SELECT date_trunc('month', timestamp '2001-02-16 20:38:40.5') AS month,
       date_trunc('week', timestamp '2001-02-16 20:38:40.5') AS week;
SELECT date_trunc('quarter', timestamp '2001-05-16') AS quarter,
       date_trunc('decade', timestamp '2001-05-16') AS decade,
       date_trunc('century', timestamp '2001-05-16') AS century;
SELECT date_trunc('milliseconds', timestamp '2001-02-16 20:38:40.123456') AS ms;
SELECT date_trunc('day', timestamp 'infinity') AS inf;

-- fields of an infinite timestamp: monotonic fields are +/-Infinity, oscillating
-- fields are NULL
SELECT date_part('year', timestamp 'infinity') AS year,
       date_part('month', timestamp 'infinity') AS month,
       extract(epoch from timestamp '-infinity') AS neg_epoch,
       extract(day from timestamp 'infinity') AS day;

-- more input forms: ISO 'T' with an attached zone, and the day-before-month and
-- full-month-name verbose spellings
SELECT timestamp '2001-09-22T18:19:20Z' AS zulu,
       timestamp '2001-09-22T18:19:20-07:00' AS offset;
SELECT timestamp '10 Feb 1997' AS dmy, timestamp 'February 10 1997' AS full_name;

-- isfinite and make_timestamp (24:00:00 rolls to the next day; sec 60 carries)
SELECT isfinite(timestamp '2001-02-16') AS finite, isfinite(timestamp 'infinity') AS not_finite;
SELECT make_timestamp(2013, 7, 15, 8, 15, 23.5) AS made,
       make_timestamp(2013, 7, 15, 8, 15, 23) AS whole;
SELECT make_timestamp(2013, 7, 15, 24, 0, 0) AS end_of_day,
       make_timestamp(2013, 7, 15, 8, 15, 60) AS leap;

-- errors: unparseable input is 22007, an unknown unit is 22023; recovery works
SELECT timestamp 'garbage';
SELECT date_part('bogus', timestamp '2001-02-16');
SELECT 'still alive' AS status;
