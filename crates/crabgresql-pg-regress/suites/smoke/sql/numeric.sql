--
-- NUMERIC
-- Arbitrary-precision numeric: input/output and display scale, the arithmetic
-- operators and their scale rules, comparisons, the rounding/scaling functions,
-- transcendentals, special values (NaN / +/-Infinity), NUMERIC(p,s) via cast,
-- and the domain/range errors. Output hand-checked against PostgreSQL's
-- psql -a -q aligned format.
--
-- literals keep their exact value and display scale (a decimal literal is
-- numeric, not float)
SELECT 1.5 AS a, 100 AS b, 0.05 AS c, 40.500000 AS d, -0.0 AS e;
SELECT 1.50e1 AS exp1, 1.5e-2 AS exp2, .5 AS bare;
-- the default column name of a numeric literal expression is ?column?
SELECT 12345.6789;

-- casts: int -> numeric (exact), float -> numeric (15/6 significant digits)
SELECT 5::numeric AS from_int, (2.0/3.0)::float8::numeric AS from_f8,
       123.456::float4::numeric AS from_f4;
-- text -> numeric and back
SELECT '3.14159'::numeric AS pi, ('3.14159'::numeric)::text AS as_text;
-- numeric -> int rounds half away from zero; .5 ties round outward
SELECT 2.5::numeric::int4 AS up, (-2.5)::numeric::int4 AS down, 2.4::numeric::int4 AS keep;
-- numeric -> int out of range, and NaN/Infinity have no integer image
SELECT 99999999999::numeric::int4;
SELECT 'NaN'::numeric::int4;
SELECT 'Infinity'::numeric::int2;
-- a malformed numeric literal, and one whose magnitude overflows the format
SELECT 'abc'::numeric;
SELECT '1e2147483647'::numeric;

-- addition / subtraction take the larger scale
SELECT 1.5 + 2.25 AS add, 4.2 - 4.2 AS zero, 0.1 + 0.2 AS tenths, 100 - 1 AS whole;
-- multiplication adds the scales
SELECT 1.10 * 1.10 AS mul, 2.5 * 2 AS half, 0 * 4.2 AS zero;
-- division gives at least 16 significant digits, honoring operand scale
SELECT 1/3.0 AS third, 4.2/4.2 AS one, 10/3.0 AS ten_thirds, 1/30000.0 AS small;
-- modulo: x - trunc(x/y)*y, at the larger scale
SELECT 5.0 % 2 AS m1, (-5.5) % 2 AS m2, 11 % 4.0 AS m3;
-- unary minus, prefix abs (@), and abs()
SELECT -(3.5) AS neg, @ (-3.5) AS at_abs, abs(-1.50) AS abs_fn;
-- division / modulo by zero
SELECT 1.0 / 0;
SELECT 1.0 % 0;

-- comparisons drive WHERE and ORDER BY; mixing numeric with int promotes to
-- numeric
SELECT 1.5 < 2 AS lt, 2.0 = 2 AS eq, 10.0 > 9.99 AS gt;
-- a numeric column: insert (including NaN and +/-Infinity), filter, and the
-- btree order where -Infinity sorts first and NaN last
CREATE TABLE num_tbl (id int4, n numeric);
INSERT INTO num_tbl VALUES (1, 2.5), (2, -3.5), (3, 'NaN'), (4, 'Infinity'),
                           (5, '-Infinity'), (6, 0);
SELECT id, n FROM num_tbl ORDER BY 2, 1;
SELECT id, n FROM num_tbl WHERE n > 0 ORDER BY 1;

-- rounding / scaling functions
SELECT round(1.5) AS r0, round(2.5) AS r_tie, round(1.5, 3) AS r3,
       round(1234.5678, -2) AS rneg;
SELECT trunc(1.6) AS t0, trunc(-1.6) AS tneg, trunc(1234.5678, 2) AS t2;
SELECT ceil(-1.5) AS c1, ceiling(1.1) AS c2, floor(-1.5) AS f1, floor(1.9) AS f2;
SELECT sign(-2.3) AS s1, sign(0.0) AS s2, sign(4.2) AS s3;
SELECT mod(11, 4.0) AS m1, mod(-5.5, 2) AS m2;
-- mod on integers keeps the integer type (only a numeric argument returns numeric)
SELECT mod(11, 4) AS int_mod, mod(-7, 3) AS neg_mod;

-- sqrt on numeric (correctly rounded) and its domain error; a numeric-typed
-- argument selects the numeric overload (an integer argument would resolve to
-- the float8 sqrt, as in PG)
SELECT sqrt(2.0) AS s2, sqrt(9.0) AS s9, sqrt(0.04) AS s04;
SELECT sqrt(-1::numeric);

-- transcendentals: ln / log / log(base, x) / exp, matching PG's result scale
SELECT ln(2.0) AS ln2, ln(0.5) AS ln_half, ln(100.0) AS ln100;
SELECT log(100.0) AS log100, log(2.0) AS log2, log(2.0, 8.0) AS log2_8;
SELECT exp(1.0) AS e, exp(0.0) AS one, exp(-1.0) AS e_inv;
-- ln domain errors (zero / negative)
SELECT ln(0.0);
SELECT ln(-1.0);

-- the ^ operator: numeric ^ numeric is numeric
SELECT 2.0 ^ 10 AS two_ten, 2.0 ^ 0.5 AS root2, 10 ^ 3.0 AS thousand;
SELECT (-2.0) ^ 3 AS negcube;
SELECT power(2.0, 0.5) AS root2, power(2.0, -10) AS tiny;
-- error cases for power
SELECT 0.0 ^ (-1);
SELECT (-2.0) ^ 0.5;

-- special values: NaN and +/-Infinity arithmetic
SELECT 'NaN'::numeric + 1 AS nan_add, 'Infinity'::numeric - 'Infinity'::numeric AS inf_sub,
       'Infinity'::numeric + 1 AS inf_add, '-Infinity'::numeric * 2 AS neg_inf;
SELECT 'NaN'::numeric = 'NaN'::numeric AS nan_eq, 'Infinity'::numeric > 1e308 AS inf_gt;
-- special-value division / modulo: inf/inf and NaN/0 are NaN, but any /0 (even
-- with an infinite dividend) is an error
SELECT 'Infinity'::numeric / 'Infinity'::numeric AS inf_inf,
       'NaN'::numeric / 0 AS nan_zero, 'Infinity'::numeric / 2 AS inf_two;
SELECT 'Infinity'::numeric / 0;
SELECT 'Infinity'::numeric % 0;
-- power with infinite operands
SELECT power(0.5, '-Infinity'::numeric) AS a, power('Infinity'::numeric, 2) AS b,
       power(2.0, '-Infinity'::numeric) AS c;

-- an integer or float argument to sqrt/ln/log resolves to the float8 overload
-- (as in PG), while a numeric argument stays numeric
SELECT sqrt(2) AS int_sqrt, ln(100) AS int_ln, log(100) AS int_log,
       abs(-2.5::float8) AS f8_abs;

-- NUMERIC(p,s) via cast: rounds to scale, and overflows the field with DETAIL
SELECT 1.005::numeric(5,2) AS rounded, 0.99994::numeric(4,4) AS fits;
SELECT 0.99995::numeric(4,4);
SELECT 12345.6::numeric(5,2);
SELECT 'Infinity'::numeric(4,4);
SELECT 'NaN'::numeric(4,4) AS nan_ok;

-- recovery after the errors above still works
SELECT 'still alive' AS status;
