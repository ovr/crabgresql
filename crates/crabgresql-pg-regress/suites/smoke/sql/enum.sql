--
-- ENUM types
-- A CREATE TYPE ... AS ENUM, used as a column type, with text<->enum casts,
-- comparisons and ORDER BY in definition (not alphabetical) order, min/max, and
-- pg_type / pg_enum catalog reflection. Output hand-checked against PostgreSQL
-- (psql -a -q).
--
CREATE TYPE rainbow AS ENUM ('red', 'orange', 'yellow', 'green', 'blue', 'purple');
-- A label may not repeat within the definition.
CREATE TYPE bad_enum AS ENUM ('a', 'b', 'a');
-- Enum text input: a valid label yields the value; an unknown one is an error.
SELECT 'red'::rainbow;
SELECT 'mauve'::rainbow;
-- Reflected into pg_type as a defined enum (typtype = 'e').
SELECT typname, typtype, typcategory, typlen, typbyval
FROM pg_type WHERE typname = 'rainbow';
-- ... and its labels into pg_enum, in definition order.
SELECT e.enumlabel, e.enumsortorder
FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid
WHERE t.typname = 'rainbow'
ORDER BY e.enumsortorder;
--
-- As a column type.
--
CREATE TABLE enumtest (id int, col rainbow);
INSERT INTO enumtest VALUES (1, 'red'), (2, 'orange'), (3, 'yellow'), (4, 'green');
-- An out-of-range label is rejected on insert.
INSERT INTO enumtest VALUES (5, 'chartreuse');
SELECT * FROM enumtest ORDER BY id;
--
-- Comparisons and ORDER BY follow definition order, not alphabetical.
--
SELECT * FROM enumtest WHERE col = 'orange';
SELECT * FROM enumtest WHERE col <> 'orange' ORDER BY id;
SELECT * FROM enumtest WHERE col > 'yellow' ORDER BY id;
SELECT * FROM enumtest WHERE col < 'green' ORDER BY id;
SELECT col FROM enumtest ORDER BY col;
--
-- Casts to and from text.
--
SELECT 'red'::rainbow::text || 'hithere';
SELECT 'blue'::text::rainbow;
--
-- Aggregates order by the enum's definition order.
--
SELECT min(col), max(col) FROM enumtest;
SELECT max(col) FROM enumtest WHERE col < 'green';
--
-- Cleanup.
--
DROP TABLE enumtest;
DROP TYPE rainbow;
SELECT COUNT(*) FROM pg_type WHERE typname = 'rainbow';
