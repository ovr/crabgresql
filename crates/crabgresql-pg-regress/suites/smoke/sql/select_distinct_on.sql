--
-- SELECT DISTINCT ON
-- Keeps the first row of each group defined by the ON expressions. The ON
-- expressions must be a prefix of ORDER BY, which decides which row wins per
-- group. ON expressions need not appear in the select list (they become hidden
-- sort columns).
--
CREATE TABLE weather (city text, day integer, temp integer);
INSERT INTO weather VALUES
  ('SF', 1, 60),
  ('SF', 2, 65),
  ('SF', 3, 55),
  ('NYC', 1, 40),
  ('NYC', 2, 50),
  ('LA', 1, 80);
-- one row per city: the lowest day (ON prefix of ORDER BY)
SELECT DISTINCT ON (city) city, day, temp FROM weather ORDER BY city, day;
-- pick the warmest day per city with a DESC tiebreak
SELECT DISTINCT ON (city) city, day, temp FROM weather ORDER BY city, temp DESC;
-- ON expression not in the select list: a hidden ordering column
SELECT DISTINCT ON (city) temp FROM weather ORDER BY city, day;
-- error: ON expressions must match the initial ORDER BY expressions
SELECT DISTINCT ON (city) city, day FROM weather ORDER BY day;
DROP TABLE weather;
