--
-- CAST
-- CAST(expr AS type) and expr::type across the M0 scalar types, plus the
-- error rendering on a bad cast. Output hand-checked against PostgreSQL's
-- psql -a -q aligned format.
--
-- unknown literals take the cast's target type; both spellings are equivalent
SELECT '123'::int4 AS i, 't'::bool AS b, '1.5'::float8 AS f;
SELECT CAST('42' AS int8) AS big, CAST('-7' AS int2) AS small;
-- integer -> numeric (exact) and float -> numeric (15 significant digits)
SELECT 5::int4::numeric AS to_num, (1.5::float8)::numeric AS half,
       (2::float8 / 3::float8)::numeric AS thirds;
-- numeric -> integer rounds half away from zero
SELECT 12.5::numeric::int4 AS up, (-12.5)::numeric::int4 AS down;
-- casts run per row over a text column
CREATE TABLE labels (s text);
INSERT INTO labels VALUES ('10'), ('20'), ('-3');
SELECT s, s::int4 * 2 AS doubled FROM labels;
-- a bad runtime cast reports PG's message and SQLSTATE; the session recovers
CREATE TABLE oops (s text);
INSERT INTO oops VALUES ('abc');
SELECT s::int4 FROM oops;
SELECT 'NaN'::numeric::int4;
SELECT 'alive' AS status;
