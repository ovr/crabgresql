--
-- GENERATE_SERIES
-- The set-returning function generate_series over int4/int8/numeric and
-- timestamp/timestamptz (stepped by an interval), in both the target list and
-- FROM position. Output hand-checked against PostgreSQL's aligned format.
--
-- Render timestamptz in UTC so the output is deterministic.
SET timezone = 'UTC';
-- target-list form: one row per value in the range
SELECT generate_series(1, 5);
-- FROM position: the column is named after the function
SELECT * FROM generate_series(1, 5);
-- an explicit step
SELECT generate_series(2, 10, 2);
-- a descending series needs a negative step
SELECT generate_series(5, 1, -1);
-- a single-element range
SELECT generate_series(3, 3);
-- a step in the wrong direction (or start past stop) yields no rows
SELECT generate_series(5, 1);
SELECT generate_series(1, 5, -1);
-- the series widens to int8 when a bound does not fit int4
SELECT generate_series(4000000000, 4000000002);
-- filtering and ordering over the FROM-position rows
SELECT generate_series FROM generate_series(1, 6) WHERE generate_series % 2 = 0;
SELECT * FROM generate_series(1, 4) ORDER BY 1 DESC;
-- a column-list alias in FROM renames the single output column
SELECT g FROM generate_series(1, 3) AS s(g) ORDER BY 1;
-- a bare alias renames it too: the alias names both the relation and, because
-- the function returns a scalar, its one column. The AS is optional.
SELECT i FROM generate_series(1, 3) AS i ORDER BY 1;
SELECT i FROM generate_series(1, 3) i ORDER BY 1;
SELECT * FROM generate_series(1, 3) AS i;
-- the qualified spelling still works, and two aliased series can be joined
SELECT i.i FROM generate_series(1, 2) AS i ORDER BY 1;
SELECT x, y FROM generate_series(1, 2) x, generate_series(1, 2) y ORDER BY x, y;
-- a column list still wins over the bare alias
SELECT g FROM generate_series(1, 3) AS i(g) ORDER BY 1;
-- more column aliases than the function produces is an error
SELECT * FROM generate_series(1, 3) AS s(a, b);
-- numeric overload: a fractional step; the start keeps its scale
SELECT generate_series(1, 3, 0.5);
-- numeric two-arg form defaults the step to 1
SELECT generate_series(1.5, 3);
-- numeric counting down with a negative step
SELECT generate_series(3.0, 1.0, -0.5);
-- timestamp stepped by an interval (3-arg only)
SELECT * FROM generate_series(timestamp '2020-01-01', timestamp '2020-01-04', interval '1 day');
-- a whole-month step clamps the day of month, incrementally
SELECT generate_series(timestamp '2020-01-31', timestamp '2020-04-30', interval '1 month');
-- timestamptz (UTC session) stepped by an interval
SELECT generate_series(timestamptz '2020-01-01 00:00+00', timestamptz '2020-01-03 00:00+00', interval '1 day');
-- a zero step is an error, for every overload
SELECT generate_series(1, 5, 0);
SELECT generate_series(1.0, 3.0, 0.0);
SELECT generate_series(timestamp '2020-01-01', timestamp '2020-01-05', interval '0');
-- NaN and infinite numeric bounds/step are errors
SELECT generate_series('NaN'::numeric, 3);
SELECT generate_series(1, 'infinity'::numeric);
SELECT generate_series(1, 5, 'infinity'::numeric);
-- but a NULL argument short-circuits to no rows before any such validation
SELECT generate_series(NULL::int, 5, 0);
SELECT generate_series(NULL::numeric, 'NaN'::numeric);
