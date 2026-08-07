--
-- DATE
-- date: input parsing, ISO output, comparisons, casts, integer/interval
-- arithmetic, and the field functions (date_part / extract / isfinite /
-- make_date). Output hand-checked against PostgreSQL's aligned format.
--
-- typed-literal output; the ISO and verbose input forms
SELECT date '1999-01-08' AS iso, date 'January 8, 1999' AS verbose;
SELECT date '2000-02-29' AS leap, date '0044-03-15 BC' AS bc;
-- special values
SELECT date 'infinity' AS pos, date '-infinity' AS neg, date 'epoch' AS unix;
-- the default column name of a typed literal is the type name
SELECT date '2020-08-11';

-- comparisons drive WHERE and ORDER BY; the infinity sentinels sort naturally
SELECT date '2000-01-01' = date '2000-01-01' AS eq,
       date '1999-01-08' < date '2000-01-01' AS lt,
       date '-infinity' < date '2000-01-01' AS neg_first,
       date 'infinity' > date '2000-01-01' AS pos_last;

-- casts both directions; date widens to timestamp, a timestamp narrows to its date
SELECT '2020-08-11'::date AS from_text;
SELECT (date '2020-08-11')::text AS to_text;
SELECT (date '2020-08-11')::timestamp AS to_ts;
SELECT (timestamp '2020-08-11 13:30:00')::date AS from_ts;

-- a date column: insert, filter, order
CREATE TABLE date_tbl (id int4, d date);
INSERT INTO date_tbl VALUES (1, '1957-04-09'), (2, '1996-02-29'), (3, '2000-01-01'), (4, '2040-04-10');
SELECT id, d FROM date_tbl WHERE d < date '2000-01-01' ORDER BY 2;
SELECT id FROM date_tbl WHERE d >= date '1996-01-01' AND d <= date '2001-01-01' ORDER BY 1;

-- simple math: date - date -> int, date +/- int -> date, date +/- interval -> timestamp
SELECT date '2000-01-02' - date '2000-01-01' AS one_day;
SELECT date '2000-01-01' + 31 AS plus_31, date '2000-03-01' - 1 AS minus_1;
SELECT date '2001-01-01' + interval '1 day 2 hours' AS plus_interval;
SELECT date '2001-01-01' - interval '1 day' AS minus_interval;
SELECT date '2020-08-11' + time '13:30:00' AS plus_time;
-- date - timestamp -> interval (date widens to midnight); date + timetz -> timestamptz
SELECT date '2020-01-02' - timestamp '2020-01-01 12:00' AS d_minus_ts;
SELECT date '2020-08-11' + timetz '13:30:00-04' AS d_plus_timetz;

-- date_part (float8) across the supported fields
SELECT date_part('year', date '2020-08-11') AS year,
       date_part('month', date '2020-08-11') AS month,
       date_part('day', date '2020-08-11') AS day;
SELECT date_part('quarter', date '2020-08-11') AS quarter,
       date_part('week', date '2020-08-11') AS week,
       date_part('isoyear', date '2020-08-11') AS isoyear;
SELECT date_part('dow', date '2020-08-11') AS dow,
       date_part('isodow', date '2020-08-11') AS isodow,
       date_part('doy', date '2020-08-11') AS doy;
SELECT date_part('decade', date '2020-08-11') AS decade,
       date_part('century', date '2020-08-11') AS century,
       date_part('millennium', date '2020-08-11') AS millennium;
-- extract (numeric): epoch and julian are integer-valued for a date
SELECT extract(epoch from date '2020-08-11') AS epoch,
       extract(julian from date '2020-08-11') AS julian;
SELECT extract(year from date '2020-08-11 BC') AS bc_year;
-- non-finite date: monotonic fields -> +/-Infinity, oscillating -> NULL
SELECT extract(epoch from date 'infinity') AS pos_epoch,
       extract(year from date 'infinity') AS pos_year;
SELECT extract(day from date 'infinity') AS osc_day;

-- isfinite and make_date
SELECT isfinite(date '2020-08-11') AS finite, isfinite(date 'infinity') AS inf;
SELECT make_date(2013, 7, 15) AS mk, make_date(-44, 3, 15) AS mk_bc;

-- errors: bad input (22007), field out of range (22008), bad unit (22023);
-- recovery still works
SELECT date 'garbage';
SELECT date '2000-02-30';
-- a date past the timestamp range widens with a clean error, not a crash
SELECT (date '1000000-01-01')::timestamp;
-- date - (int MIN) does not overflow the negation; reports out of range
SELECT date '2000-01-01' - (-2147483648);
-- there is no date + bigint operator
SELECT date '2000-01-01' + 5::int8;
SELECT extract(hour from date '2020-08-11');
SELECT extract(fortnight from date '2020-08-11');
SELECT make_date(2013, 2, 30);
SELECT 'still alive' AS status;
