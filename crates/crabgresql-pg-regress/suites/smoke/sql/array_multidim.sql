--
-- ARRAY_MULTIDIM
-- Multi-dimensional arrays: nested-brace literals and their output, the
-- `[lower:upper]=` bound prefix, the `ARRAY[[...]]` constructor, subscript
-- chains, the shape-reporting functions, durable storage, and how the array
-- operators behave once a value has more than one dimension.
--
-- An array's dimensionality belongs to the *value*, so `int[]` and `int[][]`
-- name the same type and the same column accepts either shape.
--
-- nested-brace literal, and the constructor spellings that build the same value
SELECT '{{1,2,3},{4,5,6}}'::int[];
SELECT ARRAY[[1, 2, 3], [4, 5, 6]];
SELECT ARRAY[ARRAY[1, 2, 3], ARRAY[4, 5, 6]];
SELECT pg_typeof(ARRAY[[1, 2]]);
-- whitespace between the braces is insignificant
SELECT ' { {1,2} , {3,4} } '::int[];
-- three dimensions, and text elements that need quoting
SELECT ARRAY[[[1, 2]], [[3, 4]]];
SELECT '{{a,"b,c"},{NULL,""}}'::text[];

-- the shape-reporting functions
SELECT array_ndims(ARRAY[[1, 2], [3, 4]]) AS ndims,
       array_dims(ARRAY[[1, 2], [3, 4]]) AS dims,
       array_length(ARRAY[[1, 2], [3, 4]], 1) AS len1,
       array_length(ARRAY[[1, 2], [3, 4]], 2) AS len2,
       array_length(ARRAY[[1, 2], [3, 4]], 3) AS len3,
       array_lower(ARRAY[[1, 2], [3, 4]], 2) AS lower2,
       array_upper(ARRAY[[1, 2], [3, 4]], 2) AS upper2,
       cardinality(ARRAY[[1, 2], [3, 4]]) AS card;
-- an empty array has no dimensions at all, so every one of them is NULL
SELECT array_ndims('{}'::int[]) AS ndims,
       array_dims('{}'::int[]) AS dims,
       array_length('{}'::int[], 1) AS len,
       array_lower('{}'::int[], 1) AS lower,
       cardinality('{}'::int[]) AS card;
-- however many brace levels the literal spells it with
SELECT '{{}}'::int[], array_dims('{{},{}}'::int[]);
-- the vectors keep their own conventions: one dimension, lower bound 0
SELECT array_ndims('11 22'::oidvector) AS ndims,
       array_dims('11 22'::oidvector) AS dims,
       array_dims(''::oidvector) AS empty_dims,
       array_lower('11 22'::oidvector, 1) AS lower;

-- subscripting: one index per dimension
SELECT (ARRAY[[1, 2], [3, 4]])[1][2] AS elem,
       (ARRAY[[1, 2], [3, 4]])[2][1] AS elem2;
-- too few or too many subscripts is NULL, as is one out of range
SELECT (ARRAY[[1, 2], [3, 4]])[1] AS too_few,
       (ARRAY[[1, 2], [3, 4]])[1][2][3] AS too_many,
       (ARRAY[[1, 2], [3, 4]])[1][5] AS out_of_range,
       (ARRAY[[1, 2], [3, 4]])[NULL][1] AS null_subscript;

-- the `[lower:upper]=` prefix fixes the subscript bounds, and prints back out
SELECT '[2:3]={1,2}'::int[] AS shifted,
       ('[2:3]={1,2}'::int[])[2] AS at_lower,
       ('[2:3]={1,2}'::int[])[1] AS below_lower;
SELECT '[2:3][0:1]={{1,2},{3,4}}'::int[] AS shifted2,
       ('[2:3][0:1]={{1,2},{3,4}}'::int[])[3][0] AS elem;
-- at the default bounds the prefix is dropped again
SELECT '[1:2]={1,2}'::int[];
-- a dimension may name only its upper bound, and the spellings mix
SELECT '[3]={1,2,3}'::int[] AS upper_only,
       '[1:2][3]={{1,2,3},{4,5,6}}'::int[] AS mixed,
       array_dims('[3][2]={{1,2},{3,4},{5,6}}'::int[]) AS dims;
-- signed bounds
SELECT '[-2:-1]={1,2}'::int[] AS negative, '[+2:3]={1,2}'::int[] AS plus_signed;
-- whitespace is allowed between the bracket pairs and around the `=`
SELECT '[2:3] = {1,2}'::int[] AS spaced,
       '[1:2] [1:2]={{1,2},{3,4}}'::int[] AS spaced_between;
-- but not inside a bracket pair
SELECT '[ 2 : 3 ]={1,2}'::int[];
SELECT '[a:b]={1,2}'::int[];
SELECT '[]={1,2}'::int[];
SELECT '[2:3'::int[];
SELECT '[1:2]'::int[];
-- dimensions read, but nothing to assign them to
SELECT '[2:3]='::int[];
-- the bounds are part of the value: same elements, different array
SELECT '[2:3]={1,2}'::int[] = '{1,2}'::int[] AS eq,
       '[2:3]={1,2}'::int[] > '{1,2}'::int[] AS gt,
       ARRAY[[1, 2]] = ARRAY[1, 2] AS eq_ndims,
       ARRAY[[1, 2]] > ARRAY[1, 2] AS gt_ndims,
       ARRAY[[1, 2]] > ARRAY[1, 2, 3] AS elements_first;
-- every dimension's length outranks every lower bound, so the 1x4 array below
-- sorts under the 2x2 one despite its higher first lower bound
SELECT '[5:5][1:4]={{1,2,3,4}}'::int[] > '{{1,2},{3,4}}'::int[] AS lengths_first,
       '[1:2][5:6]={{1,2},{3,4}}'::int[] > '[1:2][1:2]={{1,2},{3,4}}'::int[] AS then_bounds;

-- unnest and generate_subscripts read the value row-major, dimension by dimension
SELECT unnest(ARRAY[[1, 2], [3, 4]]);
SELECT generate_subscripts(ARRAY[[1, 2, 3], [4, 5, 6]], 2);
SELECT generate_subscripts(ARRAY[[1, 2], [3, 4]], 3) AS no_such_dimension;

-- concatenation joins along the first dimension; a lower-dimensional operand is
-- one slice of the other, and the bounds come from the higher-dimensional side
SELECT ARRAY[[1, 2], [3, 4]] || ARRAY[[5, 6]] AS same_ndims,
       ARRAY[[1, 2]] || ARRAY[3, 4] AS slice_appended,
       ARRAY[1, 2] || ARRAY[[3, 4]] AS slice_prepended,
       array_dims('[2:3][1:2]={{1,2},{3,4}}'::int[] || ARRAY[5, 6]) AS bounds_kept;
-- an empty or NULL operand contributes nothing, not even a shape
SELECT array_cat('{}'::int[], ARRAY[[1, 2]]) AS empty_left,
       array_cat(NULL::int[], ARRAY[[1, 2]]) AS null_left;
-- mismatched shapes are rejected
SELECT array_cat(ARRAY[[1, 2]], ARRAY[[3, 4, 5]]);
SELECT array_cat(ARRAY[[1, 2]], ARRAY[3, 4, 5]);
SELECT array_cat(ARRAY[[[1]]], ARRAY[1]);
-- appending a single element only makes sense along one dimension
SELECT array_append(ARRAY[[1, 2], [3, 4]], 5);
SELECT ARRAY[[1, 2]] || 5;
SELECT array_prepend(5, ARRAY[[1, 2]]);
-- but the bounds of a one-dimensional array do survive it
SELECT array_append('[4:5]={1,2}'::int[], 9) AS appended,
       array_prepend(9, '[4:5]={1,2}'::int[]) AS prepended;

-- containment and the quantified comparisons look at every element, whatever
-- the shape; array_to_string renders them row-major
SELECT ARRAY[[1, 2], [3, 4]] @> ARRAY[3] AS contains,
       3 = ANY (ARRAY[[1, 2], [3, 4]]) AS any_match,
       array_to_string(ARRAY[[1, 2], [3, 4]], '-') AS joined;

-- malformed literals
SELECT '{{1,2},{3}}'::int[];
SELECT '{{1,2},3}'::int[];
SELECT '{1,{2}}'::int[];
SELECT '{{{{{{{1}}}}}}}'::int[];
SELECT '[1:3]={1,2}'::int[];
SELECT '[1:2]{1,2}'::int[];
SELECT '[2:1]={}'::int[];
-- a constructor whose operands disagree fails at run time, not at parse time
SELECT ARRAY[ARRAY[1, 2], ARRAY[3]];
-- the constructor is capped at six dimensions too, and names the count it was
-- asked for where the literal parser does not
SELECT array_ndims(ARRAY[ARRAY[ARRAY[ARRAY[ARRAY[ARRAY[1]]]]]]) AS six_is_fine;
SELECT ARRAY[ARRAY[ARRAY[ARRAY[ARRAY[ARRAY[ARRAY[1]]]]]]];

-- a column declared `int[][]` is the same type as `int[]` and stores either
CREATE TABLE md (id int, a int[][]);
INSERT INTO md VALUES (1, '{{1,2},{3,4}}');
INSERT INTO md VALUES (2, ARRAY[[5, 6, 7], [8, 9, 10]]);
INSERT INTO md VALUES (3, ARRAY[11, 12]);
INSERT INTO md VALUES (4, '[0:1][2:3]={{1,2},{3,4}}');
SELECT id, a, array_dims(a), a[1][2] FROM md ORDER BY id;
SELECT id FROM md WHERE a[2][1] = 3;
SELECT a FROM md ORDER BY a;
DROP TABLE md;
