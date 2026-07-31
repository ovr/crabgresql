--
-- GEO
-- point / lseg: input parsing (the accepted spellings), text output, casts to
-- and from text, the positional / distance / arithmetic operators, the lseg
-- accessors, and the invalid-input errors. Output hand-checked against
-- PostgreSQL's aligned format. extra_float_digits is pinned so float output is
-- stable across platforms.
--
SET extra_float_digits = 0;

-- typed-literal output; the default column name of a typed literal is the type
SELECT point '(1,2)', lseg '[(1,2),(3,4)]';

-- point input: parens optional, whitespace tolerant, Inf/NaN accepted
SELECT '(0,0)'::point AS a, '10,10'::point AS b, ' ( -3.0 , 4.0 ) '::point AS c;

-- lseg input: bracketed, doubly-parenthesized, bare pairs, and bracketed scalars
SELECT '[(1,2),(3,4)]'::lseg AS a,
       '((0,0),(6,6))'::lseg AS b,
       '10,-10 ,-3,-4'::lseg AS c,
       '[-1e6,2e2,3e5, -4e1]'::lseg AS d;

-- a bare coordinate list inside a single pair of parens: the leading '(' is a
-- grouping paren because it is the last '(' in the string, not the first point's
SELECT '(1,2,3,4)'::lseg AS a, '( 11,12,13,14) '::lseg AS b;

-- casts to text follow the type-name column-naming rule; lseg::point is center
SELECT (point '(5.1,34.5)')::text, (lseg '[(1,2),(3,4)]')::point;

-- a point column round-trips every insert spelling
CREATE TABLE point_tbl(f1 point);
INSERT INTO point_tbl VALUES ('(0,0)'), ('(-10,0)'), ('(-3,4)'),
  ('(5.1, 34.5)'), ('(-5,-12)'), ('10,10');
SELECT * FROM point_tbl;

-- positional predicates
SELECT f1 FROM point_tbl WHERE f1 << '(0,0)';
SELECT f1 FROM point_tbl WHERE f1 >> '(0,0)';
SELECT f1 FROM point_tbl WHERE f1 |>> '(0,0)';
SELECT f1 FROM point_tbl WHERE f1 <<| '(0,0)';
SELECT f1 FROM point_tbl WHERE f1 ~= '(5.1,34.5)';

-- distance, sorted (a float8 scalar drives ORDER BY; the point itself does not)
SELECT f1, f1 <-> point '(0,0)' AS dist FROM point_tbl ORDER BY dist, f1 <-> point '(0,0)';

-- point arithmetic: translate, complex multiply / divide
SELECT point '(5.1,34.5)' + point '(1,1)' AS add,
       point '(5.1,34.5)' - point '(1,1)' AS sub,
       point '(5.1,34.5)' * point '(-10,0)' AS mul,
       point '(10,10)' / point '(10,10)' AS div;

-- divide by the zero point errors
SELECT point '(1,1)' / point '(0,0)';

-- horizontal / vertical predicates and slope / constructor functions
SELECT point '(1,5)' ?- point '(9,5)' AS horiz, point '(5,1)' ?| point '(5,9)' AS vert;
SELECT slope(point '(0,0)', point '(2,1)') AS slope, point(3, 4) AS constructed;

-- lseg column and the accessors: length, center, vertical / horizontal
CREATE TABLE lseg_tbl(s lseg);
INSERT INTO lseg_tbl VALUES ('[(1,2),(3,4)]'), ('[(0,0),(6,6)]'),
  ('[(-10,2),(-10,3)]'), ('[(0,-20),(30,-20)]'), (lseg(point(11,22), point(33,44)));
SELECT s, @-@ s AS length, @@ s AS center FROM lseg_tbl;
SELECT s FROM lseg_tbl WHERE ?| s;
SELECT s FROM lseg_tbl WHERE ?- s;

-- point-to-segment distance and on-segment containment
SELECT point '(0,1)' <-> lseg '[(0,0),(1,0)]' AS dist_ps,
       point '(0,5)' ## lseg '[(0,0),(10,0)]' AS closest,
       point '(3,0)' <@ lseg '[(0,0),(10,0)]' AS on_seg;

-- segment-to-segment: distance, intersection, parallel / perpendicular, order
SELECT lseg '[(0,0),(1,0)]' <-> lseg '[(0,2),(1,2)]' AS dist_ss,
       lseg '[(0,0),(2,0)]' # lseg '[(1,-1),(1,1)]' AS intersect,
       lseg '[(0,0),(2,0)]' # lseg '[(0,1),(2,1)]' AS no_intersect;
SELECT lseg '[(0,0),(1,0)]' ?|| lseg '[(0,2),(1,2)]' AS parallel,
       lseg '[(0,0),(1,0)]' ?-| lseg '[(0,0),(0,1)]' AS perpendicular;
SELECT lseg '[(0,0),(2,0)]' < lseg '[(0,0),(3,0)]' AS shorter,
       lseg '[(0,0),(2,0)]' = lseg '[(0,0),(2,0)]' AS eq;

-- path: a column round-trips open and closed spellings. (Upstream path.sql
-- covers the input grammar and isopen/isclosed/popen/pclose; the rest of this
-- section is the operator surface upstream does not exercise.)
CREATE TABLE path_tbl(p path);
INSERT INTO path_tbl VALUES ('[(1,2),(3,4)]'), ('((1,2),(3,4))'),
  ('[(0,0),(3,0),(4,5),(1,6)]'), ('((10,20))'), ('1,2,3,4');
SELECT p, # p AS npoints, @-@ p AS length, area(p) AS area FROM path_tbl;

-- the function spellings of the same three, plus the open/closed conversions
SELECT npoints(p) AS npoints, length(p) AS length,
       isopen(p) AS isopen, isclosed(p) AS isclosed
  FROM path_tbl;

-- a closed path adds the segment back to the first point; an open one has no area
SELECT @-@ path '[(0,0),(3,0),(3,4)]' AS len_open,
       @-@ path '((0,0),(3,0),(3,4))' AS len_closed,
       area(path '((0,0),(4,0),(4,3))') AS area_closed,
       area(path '[(0,0),(4,0),(4,3)]') AS area_open;

-- concatenation joins two open paths; a closed operand yields NULL
SELECT path '[(0,0),(1,0)]' + path '[(2,2),(3,3)]' AS concat,
       path '((0,0),(1,0))' + path '[(2,2),(3,3)]' AS closed_left,
       path '[(0,0),(1,0)]' + path '((2,2),(3,3))' AS closed_right;

-- a point operand translates / rotates / scales every vertex
SELECT path '[(1,2),(3,4)]' + point '(1,1)' AS add,
       path '[(1,2),(3,4)]' - point '(1,1)' AS sub,
       path '[(1,2),(3,4)]' * point '(2,0)' AS mul,
       path '[(1,2),(3,4)]' / point '(2,0)' AS div;
SELECT path '[(1,2),(3,4)]' / point '(0,0)';

-- distance to another path and to a point, and outline intersection
SELECT path '[(0,0),(1,1)]' <-> path '[(3,0),(4,1)]' AS dist_pp,
       path '((0,0),(1,0),(1,1))' <-> path '((5,0),(6,0))' AS dist_closed,
       path '[(0,0),(1,1)]' <-> point '(0,3)' AS dist_pt,
       point '(0,3)' <-> path '[(0,0),(1,1)]' AS dist_pt_rev;
SELECT path '[(0,0),(2,0)]' ?# path '[(1,-1),(1,1)]' AS crosses,
       path '[(0,0),(2,0)]' ?# path '[(0,1),(2,1)]' AS misses;

-- containment: only a closed path encloses points, and its boundary counts
SELECT path '((0,0),(4,0),(4,4),(0,4))' @> point '(1,1)' AS inside,
       path '((0,0),(4,0),(4,4),(0,4))' @> point '(0,2)' AS on_edge,
       path '((0,0),(4,0),(4,4),(0,4))' @> point '(9,9)' AS outside,
       path '[(0,0),(4,0),(4,4),(0,4)]' @> point '(1,1)' AS open_encloses_nothing;
SELECT point '(1,0)' <@ path '[(0,0),(2,0)]' AS on_outline,
       point '(1,1)' <@ path '[(0,0),(4,0),(4,4),(0,4)]' AS off_outline,
       point '(1,3)' <@ path '((0,0),(2,0),(2,6))' AS on_closing_seg;

-- path comparisons look at the number of points only, not the coordinates
SELECT path '[(0,0),(1,1)]' = path '((5,5),(6,6))' AS eq,
       path '[(0,0),(1,1),(2,2)]' > path '((5,5),(6,6))' AS gt,
       path '[(0,0),(1,1)]' < path '[(0,0),(1,1),(2,2)]' AS lt;

-- invalid input is 22P02, echoing the offending text; the coordinate out of
-- range keeps float8's "double precision" range error
SELECT '(10.0 10.0)'::point;
SELECT '(10.0, 1e+500)'::point;
SELECT '[(1,2),(3)]'::lseg;
SELECT '[]'::path;

-- the non-error-throwing input API
SELECT pg_input_is_valid('(1,2)', 'point') AS ok, pg_input_is_valid('1,y', 'point') AS bad;
SELECT * FROM pg_input_error_info('1,y', 'point');
