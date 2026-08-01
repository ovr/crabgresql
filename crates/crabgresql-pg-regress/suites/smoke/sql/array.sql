--
-- ARRAY
-- One-dimensional arrays: the `{...}` text format, `ARRAY[...]` and `'{...}'`
-- literals, element subscripting, durable storage round-trip, element-wise
-- ordering, and the common array operators/functions. Output hand-checked
-- against PostgreSQL's aligned format.
--
-- ARRAY[...] constructor; the default column name of a constructor is "array"
SELECT ARRAY[1, 2, 3];
-- text elements; delimiters/empty/NULL-lookalikes are quoted on output
SELECT ARRAY['a', 'b,c', 'NULL', ''];
-- '{...}' literal cast to an element type
SELECT '{1,2,3}'::int[];
SELECT '{a,b,c}'::text[];
-- an empty array and a NULL element
SELECT ARRAY[]::int[] AS empty, ARRAY[1, NULL, 3] AS with_null;

-- element subscripting is 1-based; out-of-range and NULL subscripts are NULL
SELECT (ARRAY[10, 20, 30])[1] AS first,
       (ARRAY[10, 20, 30])[3] AS last,
       (ARRAY[10, 20, 30])[0] AS below,
       (ARRAY[10, 20, 30])[9] AS above;

-- storage round-trip through an array column, plus subscript in the target list
CREATE TABLE arr (id int, a int[], t text[]);
INSERT INTO arr VALUES (1, ARRAY[1, 2, 3], ARRAY['x', 'y']);
INSERT INTO arr VALUES (2, '{4,5}', '{z}');
INSERT INTO arr VALUES (3, ARRAY[1, NULL, 3], NULL);
SELECT id, a, t, a[1] AS a1 FROM arr ORDER BY id;

-- element-wise ordering (shorter array is less on a common prefix)
SELECT a FROM (VALUES
    (ARRAY[1, 2, 3]),
    (ARRAY[1, 2]),
    (ARRAY[1, 3]),
    (ARRAY[1, 2, 0])
) AS v(a) ORDER BY a;

-- equality and comparison
SELECT ARRAY[1, 2] = ARRAY[1, 2] AS eq,
       ARRAY[1, 2] = ARRAY[1, 3] AS ne,
       ARRAY[1, 2] < ARRAY[1, 3] AS lt;

-- concatenation: array || array, array || element, element || array
SELECT ARRAY[1, 2] || ARRAY[3, 4] AS arrs,
       ARRAY[1, 2] || 3 AS append,
       0 || ARRAY[1, 2] AS prepend;

-- containment and overlap
SELECT ARRAY[1, 2, 3] @> ARRAY[2, 3] AS contains,
       ARRAY[2, 3] <@ ARRAY[1, 2, 3] AS contained,
       ARRAY[1, 2] && ARRAY[2, 3] AS overlap,
       ARRAY[1, 2] && ARRAY[3, 4] AS no_overlap;

-- array_length / cardinality (empty array length on dim 1 is NULL)
SELECT array_length(ARRAY[1, 2, 3], 1) AS len,
       array_length(ARRAY[]::int[], 1) AS empty_len,
       cardinality(ARRAY[1, 2, 3]) AS card;

-- array_append / array_prepend / array_cat (append of NULL keeps the NULL)
SELECT array_append(ARRAY[1, 2], 3) AS appended,
       array_prepend(0, ARRAY[1, 2]) AS prepended,
       array_cat(ARRAY[1, 2], ARRAY[3, 4]) AS catted,
       array_append(ARRAY[1, 2], NULL) AS append_null;

-- unnest expands an array to a set of rows
SELECT unnest(ARRAY[10, 20, 30]);
SELECT unnest(ARRAY['a', 'b']) AS u;
-- in FROM position, where the alias names the output column
SELECT * FROM unnest(ARRAY[10, 20, 30]);
SELECT u FROM unnest(ARRAY[10, 20, 30]) AS u ORDER BY u DESC;
SELECT e FROM unnest(ARRAY[1, 2]) AS t(e) ORDER BY 1;
-- NULL elements come through as rows
SELECT u FROM unnest(ARRAY[1, NULL, 3]) AS u;

DROP TABLE arr;

-- containment/overlap treat NULL elements as matching nothing
SELECT ARRAY[NULL]::int[] @> ARRAY[NULL]::int[] AS null_contains,
       ARRAY[1, NULL]::int[] @> ARRAY[NULL]::int[] AS null_contains2,
       ARRAY[1, NULL]::int[] && ARRAY[2, NULL]::int[] AS null_overlap;

-- `||` with an untyped literal or NULL concatenates arrays (not append)
SELECT ARRAY[1, 2] || '{3,4}' AS lit_concat,
       ARRAY[1, 2] || NULL AS null_right,
       NULL || ARRAY[1, 2] AS null_left,
       ARRAY['a', 'b'] || '{c,d}' AS text_concat;

-- element types are promoted (int + numeric -> numeric[]), like PG
SELECT ARRAY[1, 2] || 3.5 AS append_promote,
       array_cat(ARRAY[1, 2], '{3.9}'::numeric[]) AS cat_promote,
       array_append(ARRAY[1, 2], 3.5) AS fn_append_promote;

-- VALUES / CASE unify differing but compatible array element types
SELECT a FROM (VALUES (ARRAY[1]), (ARRAY[9000000000])) v(a) ORDER BY 1;
SELECT CASE WHEN true THEN ARRAY[1] ELSE ARRAY[2.5] END AS case_promote;

-- non-orderable element arrays report PG's equality-operator error, not a crash
SELECT ARRAY['1'::json] @> ARRAY['1'::json];

-- a bare, uncast empty array cannot determine its type
SELECT ARRAY[];

-- point[] columns (a non-orderable element type) store and read back
CREATE TABLE pts (id int, ps point[]);
INSERT INTO pts VALUES (1, ARRAY[point '(1,2)', point '(3,4)']);
INSERT INTO pts VALUES (2, '{"(5,6)"}');
SELECT id, ps, ps[1] FROM pts ORDER BY id;
DROP TABLE pts;
