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
-- ALTER TYPE on an enum: ADD VALUE (append, BEFORE, AFTER, IF NOT EXISTS),
-- RENAME VALUE, and RENAME TO. Changes are reflected immediately in pg_enum.
--
CREATE TYPE planets AS ENUM ('venus', 'earth', 'mars');
ALTER TYPE planets ADD VALUE 'pluto';
ALTER TYPE planets ADD VALUE 'mercury' BEFORE 'venus';
ALTER TYPE planets ADD VALUE 'jupiter' AFTER 'mars';
-- IF NOT EXISTS on an existing label skips with a NOTICE instead of erroring.
ALTER TYPE planets ADD VALUE IF NOT EXISTS 'earth';
-- A duplicate label without IF NOT EXISTS is an error.
ALTER TYPE planets ADD VALUE 'earth';
-- BEFORE/AFTER a label that does not exist is an error.
ALTER TYPE planets ADD VALUE 'nibiru' AFTER 'zeus';
-- RENAME VALUE keeps the value's position (sort order).
ALTER TYPE planets RENAME VALUE 'pluto' TO 'ceres';
-- The source label must exist and the target must not.
ALTER TYPE planets RENAME VALUE 'pluto' TO 'x';
ALTER TYPE planets RENAME VALUE 'earth' TO 'mars';
-- The full label set, in definition order. (Only the ordering is compared: PG
-- keeps a fractional enumsortorder after BEFORE/AFTER inserts, while we renumber
-- 1..N — the label sequence is identical either way.)
SELECT e.enumlabel
FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid
WHERE t.typname = 'planets'
ORDER BY e.enumsortorder;
-- New values are usable as a column and order by the enum's definition order.
CREATE TABLE solar (id int, body planets);
INSERT INTO solar VALUES (1, 'mercury'), (2, 'mars'), (3, 'ceres');
SELECT * FROM solar ORDER BY body;
-- RENAME TO renames the type itself; the old name stops resolving while the
-- column keeps working (the type OID is preserved).
ALTER TYPE planets RENAME TO bodies;
SELECT typname FROM pg_type WHERE typname IN ('planets', 'bodies');
SELECT * FROM solar ORDER BY body;
-- Renaming onto a name that already exists is an error.
ALTER TYPE bodies RENAME TO bodies;
-- Renaming a type that does not exist reports the source, even when the target
-- name would collide with a builtin.
ALTER TYPE nonesuch RENAME TO int4;
-- ADD VALUE / RENAME VALUE on a builtin (non-enum) type is a wrong-object error,
-- with PostgreSQL's spelling of the type name.
ALTER TYPE int4 ADD VALUE 'x';
ALTER TYPE text RENAME VALUE 'a' TO 'b';
--
-- Cleanup.
--
DROP TABLE solar;
DROP TYPE bodies;
DROP TABLE enumtest;
DROP TYPE rainbow;
SELECT COUNT(*) FROM pg_type WHERE typname = 'rainbow';
