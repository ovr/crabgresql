--
-- GENERATE_SERIES
-- The set-returning function generate_series over int4/int8, in both the target
-- list and FROM position. Output hand-checked against PostgreSQL's aligned
-- format.
--
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
-- a zero step is an error
SELECT generate_series(1, 5, 0);
