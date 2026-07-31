//! Geometric types: `point`, `lseg` (line segment), `path` (point list), `box`,
//! `line` (infinite), `circle` and `polygon`.
//!
//! Clean-room reproduction of PostgreSQL's observable behavior (I/O text, error
//! text, SQLSTATE) for the geometric family. This module holds pure parse /
//! format / operator helpers; the runtime `Value` representations live in
//! [`crate`] (`Value::Point([f64; 2])`, `Value::Lseg([f64; 4])`,
//! `Value::Path(`[`PathVal`]`)`, `Value::Box([f64; 4])`,
//! `Value::Line([f64; 3])`, `Value::Circle([f64; 3])`,
//! `Value::Polygon(`[`PolygonVal`]`)`).
//!
//! Coordinates are `float8`, parsed with [`crate::float::float8in`] (so bad
//! numbers get PG's `22P02`/`22003`) and formatted with
//! [`crate::float::fmt_f64`] (honoring `extra_float_digits`), matching
//! `float8out`. Comparisons are fuzzy: two coordinates within `EPSILON = 1e-6`
//! read as equal, reproducing PG's observed geometric comparison behavior
//! (e.g. `point '(0,0)' ~= point '(0.0000009,0.0000009)'` is true).

use crate::float::{f8_add, f8_div, f8_mul, f8_sub};

// SQLSTATE literals (kept local; the types crate must not depend on the wire
// crate). Mirrors `crabgresql_pg_wire::sqlstate`.
const INVALID_TEXT_REPRESENTATION: &str = "22P02";

/// The fuzz tolerance for geometric comparisons; two coordinates closer than
/// this read as equal, matching PG's observable output.
const EPSILON: f64 = 1.0e-6;

// These mirror PG's `FP*` macros *exactly*, including two details that only
// show up at the extremes and that a "difference within EPSILON" formulation
// gets wrong: `fp_eq` short-circuits on plain `==` first, so two infinities of
// the same sign are equal (their difference is NaN); and the inequalities add
// EPSILON to one side rather than subtracting the operands, so they stay
// well-defined when both sides are infinite.
fn fp_eq(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= EPSILON
}
fn fp_lt(a: f64, b: f64) -> bool {
    a + EPSILON < b
}
fn fp_le(a: f64, b: f64) -> bool {
    a <= b + EPSILON
}
fn fp_gt(a: f64, b: f64) -> bool {
    a > b + EPSILON
}
fn fp_ge(a: f64, b: f64) -> bool {
    a + EPSILON >= b
}

/// Error from geometric input or arithmetic: SQLSTATE + rendered message,
/// matching PostgreSQL's wording.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoError {
    pub sqlstate: &'static str,
    pub message: String,
}

/// `invalid input syntax for type <type>: "<orig>"` (22P02).
fn syntax(type_name: &str, orig: &str) -> GeoError {
    GeoError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type {type_name}: \"{orig}\""),
    }
}

impl From<crate::float::FloatError> for GeoError {
    fn from(e: crate::float::FloatError) -> GeoError {
        GeoError {
            sqlstate: e.sqlstate,
            message: e.message.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Input parsing: one coordinate, then an optional-parenthesized point, then a
// bracketed / grouped point list — reproducing the spellings PG accepts for
// point/lseg input (`(x,y)`, `x,y`, `[(..),(..)]`, `((..),(..))`, `x,y,x,y`).
// ---------------------------------------------------------------------------

/// A byte cursor over the (ASCII) input string. All the delimiters and float
/// tokens we consume are ASCII, so byte indexing stays on char boundaries.
struct Cur<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(s: &'a str) -> Self {
        Cur { s, i: 0 }
    }
    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && self.s.as_bytes()[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn at_end(&self) -> bool {
        self.i >= self.s.len()
    }
}

/// Length in bytes of the leading `strtod`-style float token in `rest` (0 if
/// none). Accepts an optional sign, `inf`/`infinity`/`nan` (case-insensitive),
/// or a decimal mantissa with optional exponent. Assumes no leading whitespace.
fn scan_float_token(rest: &str) -> usize {
    let b = rest.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    // `infinity` / `inf` / `nan` (case-insensitive), checked without allocating.
    let word = &b[i..];
    let starts_with_ci =
        |w: &[u8], kw: &[u8]| w.len() >= kw.len() && w[..kw.len()].eq_ignore_ascii_case(kw);
    if starts_with_ci(word, b"infinity") {
        return i + 8;
    }
    if starts_with_ci(word, b"inf") {
        return i + 3;
    }
    if starts_with_ci(word, b"nan") {
        return i + 3;
    }
    let mut saw_digit = false;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        saw_digit = true;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            saw_digit = true;
        }
    }
    if !saw_digit {
        return 0;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    i
}

/// `single_decode`: read one coordinate. On a malformed number that `strtod`
/// wouldn't advance past, returns the type's syntax error; a well-formed but
/// out-of-range number propagates float8in's `22003` "double precision" error.
fn single_decode(cur: &mut Cur, type_name: &str, orig: &str) -> Result<f64, GeoError> {
    cur.skip_ws();
    let n = scan_float_token(cur.rest());
    if n == 0 {
        return Err(syntax(type_name, orig));
    }
    let token = &cur.rest()[..n];
    let v = crate::float::float8in(token).map_err(|e| {
        // A range error keeps float8in's message; anything else is a
        // geometric-syntax error against the original string.
        if e.sqlstate == INVALID_TEXT_REPRESENTATION {
            syntax(type_name, orig)
        } else {
            GeoError {
                sqlstate: e.sqlstate,
                message: e.message,
            }
        }
    })?;
    cur.i += n;
    Ok(v)
}

/// `pair_decode`: read one point, with an optional surrounding `( )`.
fn pair_decode(cur: &mut Cur, type_name: &str, orig: &str) -> Result<[f64; 2], GeoError> {
    cur.skip_ws();
    let has_delim = cur.eat(b'(');
    let x = single_decode(cur, type_name, orig)?;
    cur.skip_ws();
    if !cur.eat(b',') {
        return Err(syntax(type_name, orig));
    }
    let y = single_decode(cur, type_name, orig)?;
    if has_delim {
        cur.skip_ws();
        if !cur.eat(b')') {
            return Err(syntax(type_name, orig));
        }
    }
    Ok([x, y])
}

/// `path_decode`: read `npts` points, honoring PG's bracket rules. `[` marks an
/// "open" path (only if `opentype`); a leading `(` immediately followed by
/// another `(` is a grouping paren; otherwise each point carries its own `()`.
/// Returns `(is_open, points)`. Requires the whole string to be consumed.
///
/// `trailing_sep` says whether a separator is tolerated *after* the final point.
/// The two callers differ here, matching PG: `lseg '[(1,2),(3,4),]'` is accepted
/// but `path '[(1,2),(3,4),]'` is a syntax error.
fn path_decode(
    orig: &str,
    opentype: bool,
    npts: usize,
    trailing_sep: bool,
    type_name: &str,
) -> Result<(bool, Vec<[f64; 2]>), GeoError> {
    let mut cur = Cur::new(orig);
    cur.skip_ws();
    let mut depth = 0;
    let mut is_open = false;
    if cur.peek() == Some(b'[') {
        if !opentype {
            return Err(syntax(type_name, orig));
        }
        is_open = true;
        depth += 1;
        cur.i += 1;
    } else if cur.peek() == Some(b'(') {
        // Look past the '(' and any whitespace: another '(' means this is a
        // grouping paren around the whole list (`((1,2),(3,4))`). So does being
        // the *last* '(' in the string, which is how a bare coordinate list
        // wrapped in one pair of parens (`(1,2,3,4)`) is spelled. Otherwise
        // this '(' belongs to the first point and each point carries its own.
        let mut j = cur.i + 1;
        let b = orig.as_bytes();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let grouping = (j < b.len() && b[j] == b'(') || !orig[cur.i + 1..].contains('(');
        if grouping {
            depth += 1;
            cur.i += 1;
        }
    }

    let mut pts = Vec::with_capacity(npts);
    for i in 0..npts {
        let p = pair_decode(&mut cur, type_name, orig)?;
        pts.push(p);
        cur.skip_ws();
        // The separator between two points is optional (`(1,2)(3,4)` parses);
        // the one after the last point is only eaten where the type allows it.
        if i + 1 < npts || trailing_sep {
            cur.eat(b',');
        }
    }

    while depth > 0 {
        cur.skip_ws();
        if cur.peek() == Some(b')') || (cur.peek() == Some(b']') && is_open && depth == 1) {
            depth -= 1;
            cur.i += 1;
        } else {
            return Err(syntax(type_name, orig));
        }
    }
    cur.skip_ws();
    if !cur.at_end() {
        return Err(syntax(type_name, orig));
    }
    Ok((is_open, pts))
}

// ---------------------------------------------------------------------------
// point
// ---------------------------------------------------------------------------

/// Parse a `point`: `(x,y)` or `x,y` (surrounding parens optional, whitespace
/// tolerant), matching the spellings PG's point input accepts.
pub fn parse_point(orig: &str) -> Result<[f64; 2], GeoError> {
    let mut cur = Cur::new(orig);
    let p = pair_decode(&mut cur, "point", orig)?;
    cur.skip_ws();
    if !cur.at_end() {
        return Err(syntax("point", orig));
    }
    Ok(p)
}

/// Format a `point` as `(x,y)`, each coordinate rendered like `float8out`.
pub fn format_point(p: &[f64; 2], efd: i32) -> String {
    format!(
        "({},{})",
        crate::float::fmt_f64(p[0], efd),
        crate::float::fmt_f64(p[1], efd)
    )
}

// ---------------------------------------------------------------------------
// lseg
// ---------------------------------------------------------------------------

/// Parse an `lseg`: `[(x1,y1),(x2,y2)]` and the other accepted spellings
/// (`((..),(..))`, `(..),(..)`, `x1,y1,x2,y2`, `[x1,y1,x2,y2]`).
pub fn parse_lseg(orig: &str) -> Result<[f64; 4], GeoError> {
    let (_open, pts) = path_decode(orig, true, 2, true, "lseg")?;
    Ok([pts[0][0], pts[0][1], pts[1][0], pts[1][1]])
}

/// Format an `lseg` as `[(x1,y1),(x2,y2)]`.
pub fn format_lseg(l: &[f64; 4], efd: i32) -> String {
    format!(
        "[({},{}),({},{})]",
        crate::float::fmt_f64(l[0], efd),
        crate::float::fmt_f64(l[1], efd),
        crate::float::fmt_f64(l[2], efd),
        crate::float::fmt_f64(l[3], efd)
    )
}

// ---------------------------------------------------------------------------
// point operators / functions
// ---------------------------------------------------------------------------

/// PG's `pg_hypot`, which is **not** the platform `hypot`: it scales by the
/// larger operand and computes `x * sqrt(1 + (y/x)^2)`. That lands one ulp away
/// from a correctly rounded `hypot` on ordinary input (`(5,5) <-> (2,2)` prints
/// as `4.24264068711929`, not `...28`), so every geometric distance has to use
/// this formulation to reproduce PG's output. Infinity wins over NaN.
fn hypot(dx: f64, dy: f64) -> f64 {
    if dx.is_infinite() || dy.is_infinite() {
        return f64::INFINITY;
    }
    if dx.is_nan() || dy.is_nan() {
        return f64::NAN;
    }
    let (mut x, mut y) = (dx.abs(), dy.abs());
    if x < y {
        std::mem::swap(&mut x, &mut y);
    }
    // Now `x >= y`, so the division below cannot divide by zero.
    if y == 0.0 {
        return x;
    }
    let yx = y / x;
    x * (1.0 + yx * yx).sqrt()
}

/// `<->` point distance.
pub fn point_distance(a: &[f64; 2], b: &[f64; 2]) -> f64 {
    hypot(a[0] - b[0], a[1] - b[1])
}

/// `<<` (a strictly left of b): `a.x < b.x`.
pub fn point_left(a: &[f64; 2], b: &[f64; 2]) -> bool {
    fp_lt(a[0], b[0])
}
/// `>>` (a strictly right of b): `a.x > b.x`.
pub fn point_right(a: &[f64; 2], b: &[f64; 2]) -> bool {
    fp_gt(a[0], b[0])
}
/// `|>>` (a strictly above b): `a.y > b.y`.
pub fn point_above(a: &[f64; 2], b: &[f64; 2]) -> bool {
    fp_gt(a[1], b[1])
}
/// `<<|` (a strictly below b): `a.y < b.y`.
pub fn point_below(a: &[f64; 2], b: &[f64; 2]) -> bool {
    fp_lt(a[1], b[1])
}
/// `~=` (same as): both coordinates fuzzily equal.
pub fn point_eq(a: &[f64; 2], b: &[f64; 2]) -> bool {
    fp_eq(a[0], b[0]) && fp_eq(a[1], b[1])
}
/// `?-` (is horizontal): the two points share a y coordinate.
pub fn point_horizontal(a: &[f64; 2], b: &[f64; 2]) -> bool {
    fp_eq(a[1], b[1])
}
/// `?|` (is vertical): the two points share an x coordinate.
pub fn point_vertical(a: &[f64; 2], b: &[f64; 2]) -> bool {
    fp_eq(a[0], b[0])
}

/// `+` translate: componentwise add (checked for float overflow).
pub fn point_add(a: &[f64; 2], b: &[f64; 2]) -> Result<[f64; 2], GeoError> {
    Ok([f8_add(a[0], b[0])?, f8_add(a[1], b[1])?])
}
/// `-` translate: componentwise subtract.
pub fn point_sub(a: &[f64; 2], b: &[f64; 2]) -> Result<[f64; 2], GeoError> {
    Ok([f8_sub(a[0], b[0])?, f8_sub(a[1], b[1])?])
}
/// `*` complex multiply: `(x1x2 - y1y2, x1y2 + y1x2)`.
pub fn point_mul(a: &[f64; 2], b: &[f64; 2]) -> Result<[f64; 2], GeoError> {
    let x = f8_sub(f8_mul(a[0], b[0])?, f8_mul(a[1], b[1])?)?;
    let y = f8_add(f8_mul(a[0], b[1])?, f8_mul(a[1], b[0])?)?;
    Ok([x, y])
}
/// `/` complex divide: `a * conj(b) / |b|^2`.
pub fn point_div(a: &[f64; 2], b: &[f64; 2]) -> Result<[f64; 2], GeoError> {
    let div = f8_add(f8_mul(b[0], b[0])?, f8_mul(b[1], b[1])?)?;
    let x = f8_div(f8_add(f8_mul(a[0], b[0])?, f8_mul(a[1], b[1])?)?, div)?;
    let y = f8_div(f8_sub(f8_mul(a[1], b[0])?, f8_mul(a[0], b[1])?)?, div)?;
    Ok([x, y])
}

/// `slope(p1, p2)`: the slope of the line through the two points. Any pair
/// sharing an x (including two equal points) yields `Infinity`, and a fuzzily
/// horizontal pair yields exactly `0`, matching PG's observed `slope()` output.
pub fn point_slope(a: &[f64; 2], b: &[f64; 2]) -> f64 {
    if fp_eq(a[0], b[0]) {
        f64::INFINITY
    } else if fp_eq(a[1], b[1]) {
        0.0
    } else {
        (a[1] - b[1]) / (a[0] - b[0])
    }
}

// ---------------------------------------------------------------------------
// lseg operators / functions
// ---------------------------------------------------------------------------

fn lseg_p0(l: &[f64; 4]) -> [f64; 2] {
    [l[0], l[1]]
}
fn lseg_p1(l: &[f64; 4]) -> [f64; 2] {
    [l[2], l[3]]
}

/// `@-@` length.
pub fn lseg_length(l: &[f64; 4]) -> f64 {
    point_distance(&lseg_p0(l), &lseg_p1(l))
}

/// `@@` center (midpoint). Also backs the `lseg::point` cast.
pub fn lseg_center(l: &[f64; 4]) -> [f64; 2] {
    [(l[0] + l[2]) / 2.0, (l[1] + l[3]) / 2.0]
}

/// `?|` vertical: the two endpoints share an x.
pub fn lseg_vertical(l: &[f64; 4]) -> bool {
    fp_eq(l[0], l[2])
}
/// `?-` horizontal: the two endpoints share a y.
pub fn lseg_horizontal(l: &[f64; 4]) -> bool {
    fp_eq(l[1], l[3])
}

/// `=` endpoints fuzzily equal (b-tree equality compares endpoints, not length).
pub fn lseg_eq(a: &[f64; 4], b: &[f64; 4]) -> bool {
    point_eq(&lseg_p0(a), &lseg_p0(b)) && point_eq(&lseg_p1(a), &lseg_p1(b))
}
/// `<>` / `!=`.
pub fn lseg_ne(a: &[f64; 4], b: &[f64; 4]) -> bool {
    !lseg_eq(a, b)
}
/// `<` by length.
pub fn lseg_lt(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_lt(lseg_length(a), lseg_length(b))
}
/// `<=` by length.
pub fn lseg_le(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_le(lseg_length(a), lseg_length(b))
}
/// `>` by length.
pub fn lseg_gt(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_gt(lseg_length(a), lseg_length(b))
}
/// `>=` by length.
pub fn lseg_ge(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_ge(lseg_length(a), lseg_length(b))
}

/// Slope of a non-vertical segment (`dy/dx`). Callers must guard against a
/// vertical segment first, where the slope is undefined.
fn seg_slope(l: &[f64; 4]) -> f64 {
    (l[3] - l[1]) / (l[2] - l[0])
}

/// `?||` parallel. Compared by slope (not by an absolute-scale cross product),
/// so it is coordinate-magnitude independent, matching PG: two segments with
/// the same fuzzy slope are parallel, and two vertical segments are parallel.
pub fn lseg_parallel(a: &[f64; 4], b: &[f64; 4]) -> bool {
    let (av, bv) = (lseg_vertical(a), lseg_vertical(b));
    if av || bv {
        return av && bv;
    }
    fp_eq(seg_slope(a), seg_slope(b))
}
/// `?-|` perpendicular. A vertical segment is perpendicular to a horizontal one;
/// otherwise the slopes are negative reciprocals (`slope_a * slope_b ~= -1`).
pub fn lseg_perpendicular(a: &[f64; 4], b: &[f64; 4]) -> bool {
    if lseg_vertical(a) {
        return lseg_horizontal(b);
    }
    if lseg_vertical(b) {
        return lseg_horizontal(a);
    }
    fp_eq(seg_slope(a) * seg_slope(b), -1.0)
}

/// Closest point on segment `l` to point `p`.
pub fn close_point_seg(p: &[f64; 2], l: &[f64; 4]) -> [f64; 2] {
    let (x1, y1, x2, y2) = (l[0], l[1], l[2], l[3]);
    let dx = x2 - x1;
    let dy = y2 - y1;
    let denom = dx * dx + dy * dy;
    if denom == 0.0 {
        return [x1, y1];
    }
    let mut t = ((p[0] - x1) * dx + (p[1] - y1) * dy) / denom;
    if t.is_nan() {
        // An infinite coordinate on `p` makes the projection indeterminate
        // (`inf * 0` in the dot product). PG lands on an endpoint here, which
        // keeps the resulting distance `Infinity` rather than NaN. Pick the
        // nearer endpoint, and the *second* one when they tie — which they
        // always do at infinity, and which is the one PG reports.
        return if point_distance(p, &[x2, y2]) <= point_distance(p, &[x1, y1]) {
            [x2, y2]
        } else {
            [x1, y1]
        };
    }
    if t < 0.0 {
        t = 0.0;
    } else if t > 1.0 {
        t = 1.0;
    }
    [x1 + t * dx, y1 + t * dy]
}

/// `point <-> lseg` (and `lseg <-> point`) distance.
pub fn dist_point_seg(p: &[f64; 2], l: &[f64; 4]) -> f64 {
    point_distance(p, &close_point_seg(p, l))
}

/// `point <@ lseg`: the point lies on the segment.
pub fn point_on_seg(p: &[f64; 2], l: &[f64; 4]) -> bool {
    point_eq(p, &close_point_seg(p, l))
}

/// Intersection point of two segments, if they meet in exactly one point that
/// lies on both. Backs `#`; parallel / non-touching segments yield `None`.
pub fn lseg_interpt(a: &[f64; 4], b: &[f64; 4]) -> Option<[f64; 2]> {
    // Zero-length segments — which a one-point closed `path` contributes — have
    // no direction, so the cross product below degenerates. Such a segment meets
    // another exactly where its single point lies on it; two zero-length
    // segments never meet, not even coincident ones, matching PG.
    let a_deg = point_eq(&lseg_p0(a), &lseg_p1(a));
    let b_deg = point_eq(&lseg_p0(b), &lseg_p1(b));
    if a_deg || b_deg {
        if a_deg && b_deg {
            return None;
        }
        let (pt, seg) = if a_deg {
            (lseg_p0(a), b)
        } else {
            (lseg_p0(b), a)
        };
        return point_on_seg(&pt, seg).then_some(pt);
    }
    let (x1, y1, x2, y2) = (a[0], a[1], a[2], a[3]);
    let (x3, y3, x4, y4) = (b[0], b[1], b[2], b[3]);
    let d = (x2 - x1) * (y4 - y3) - (y2 - y1) * (x4 - x3);
    if fp_eq(d, 0.0) {
        return None;
    }
    let t = ((x3 - x1) * (y4 - y3) - (y3 - y1) * (x4 - x3)) / d;
    let u = ((x3 - x1) * (y2 - y1) - (y3 - y1) * (x2 - x1)) / d;
    let on = |v: f64| v >= -EPSILON && v <= 1.0 + EPSILON;
    if on(t) && on(u) {
        Some([x1 + t * (x2 - x1), y1 + t * (y2 - y1)])
    } else {
        None
    }
}

/// `l1 ## l2`: the point on `l2` closest to `l1`. Parallel segments have no
/// single closest point, so the result is `None` (PG's `##` returns NULL). If
/// they intersect, that is the intersection point; otherwise it is the nearer
/// of `l1`'s two endpoint projections onto `l2`.
pub fn close_seg_seg(a: &[f64; 4], b: &[f64; 4]) -> Option<[f64; 2]> {
    if lseg_parallel(a, b) {
        return None;
    }
    if let Some(p) = lseg_interpt(a, b) {
        return Some(p);
    }
    // Candidates on b: projections of a's two endpoints onto b.
    let c1 = close_point_seg(&lseg_p0(a), b);
    let c2 = close_point_seg(&lseg_p1(a), b);
    let d1 = dist_point_seg(&c1, a);
    let d2 = dist_point_seg(&c2, a);
    Some(if d1 <= d2 { c1 } else { c2 })
}

/// `l1 <-> l2` segment distance (0 if they intersect).
pub fn dist_seg_seg(a: &[f64; 4], b: &[f64; 4]) -> f64 {
    if lseg_interpt(a, b).is_some() {
        return 0.0;
    }
    let mut best = dist_point_seg(&lseg_p0(a), b);
    best = best.min(dist_point_seg(&lseg_p1(a), b));
    best = best.min(dist_point_seg(&lseg_p0(b), a));
    best = best.min(dist_point_seg(&lseg_p1(b), a));
    best
}

/// `lseg(p1, p2)` constructor.
pub fn lseg_from_points(p1: &[f64; 2], p2: &[f64; 2]) -> [f64; 4] {
    [p1[0], p1[1], p2[0], p2[1]]
}

// ---------------------------------------------------------------------------
// path
// ---------------------------------------------------------------------------

/// A `path`: a non-empty list of points, either *open* (rendered
/// `[(x,y),...]`) or *closed* (rendered `((x,y),...)`). A closed path carries an
/// implicit final segment from the last point back to the first.
///
/// Unlike `point`/`lseg` this is variable length, so it is the first geometric
/// value that is not `Copy`.
#[derive(deepsize::DeepSizeOf, Clone, Debug, PartialEq)]
pub struct PathVal {
    /// Whether the last point connects back to the first.
    pub closed: bool,
    /// The vertices, in order. Always at least one.
    pub pts: Vec<[f64; 2]>,
}

impl PathVal {
    /// The segments of the path: the segment ending at each vertex, where the
    /// one ending at the first vertex exists only for a closed path (it comes
    /// from the last vertex). So a one-point *closed* path has a single
    /// degenerate zero-length segment — PG treats it as a real segment, and
    /// `<->`/`<@`/`?#` all depend on it — while a one-point *open* path has none.
    fn segments(&self) -> impl Iterator<Item = [f64; 4]> + '_ {
        let n = self.pts.len();
        let closed = self.closed;
        (0..n).filter_map(move |i| {
            let prev = if i > 0 {
                i - 1
            } else if closed {
                n - 1
            } else {
                return None;
            };
            let a = self.pts[prev];
            let b = self.pts[i];
            Some([a[0], a[1], b[0], b[1]])
        })
    }
}

/// Number of points PG reads out of a path literal: half the comma count,
/// rounding up. The decode pass then insists on consuming exactly that many
/// points and nothing more, so a malformed list still fails.
fn pair_count(s: &str) -> usize {
    let ndelim = s.bytes().filter(|&c| c == b',').count();
    ndelim.div_ceil(2)
}

/// Parse a `path`: `[(x1,y1),...]` (open), `((x1,y1),...)` (closed), and the
/// unbracketed / bare-coordinate spellings (`(1,2),(3,4)`, `1,2,3,4`).
pub fn parse_path(orig: &str) -> Result<PathVal, GeoError> {
    let npts = pair_count(orig);
    if npts == 0 {
        return Err(syntax("path", orig));
    }
    let (is_open, pts) = path_decode(orig, true, npts, false, "path")?;
    Ok(PathVal {
        closed: !is_open,
        pts,
    })
}

/// Format a `path`: `[(x,y),...]` when open, `((x,y),...)` when closed.
pub fn format_path(p: &PathVal, efd: i32) -> String {
    let (open, close) = if p.closed { ('(', ')') } else { ('[', ']') };
    let mut out = String::new();
    out.push(open);
    for (i, pt) in p.pts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format_point(pt, efd));
    }
    out.push(close);
    out
}

/// `isopen(path)`.
pub fn path_isopen(p: &PathVal) -> bool {
    !p.closed
}
/// `isclosed(path)`.
pub fn path_isclosed(p: &PathVal) -> bool {
    p.closed
}
/// `popen(path)`: the same vertices, marked open.
pub fn path_popen(p: &PathVal) -> PathVal {
    PathVal {
        closed: false,
        pts: p.pts.clone(),
    }
}
/// `pclose(path)`: the same vertices, marked closed.
pub fn path_pclose(p: &PathVal) -> PathVal {
    PathVal {
        closed: true,
        pts: p.pts.clone(),
    }
}
/// `# path` / `npoints(path)`.
pub fn path_npoints(p: &PathVal) -> i32 {
    // A path is built from a parsed literal or from other paths, so the vertex
    // count always fits an int4 in practice; saturate rather than wrap.
    i32::try_from(p.pts.len()).unwrap_or(i32::MAX)
}

/// `@-@ path` / `length(path)`: the total segment length, including the closing
/// segment of a closed path. A one-point path has length 0.
pub fn path_length(p: &PathVal) -> f64 {
    // Folded from `0.0` rather than summed: `f64`'s `Sum` identity is `-0.0`, so
    // a segment-less path would otherwise print as `-0`.
    p.segments().fold(0.0, |acc, s| acc + lseg_length(&s))
}

/// `area(path)`: the enclosed area of a closed path (the shoelace formula,
/// sign-independent). An open path has no area — PG returns NULL, so does this.
pub fn path_area(p: &PathVal) -> Option<f64> {
    if !p.closed {
        return None;
    }
    let n = p.pts.len();
    let mut area = 0.0;
    for i in 0..n {
        let a = p.pts[i];
        let b = p.pts[(i + 1) % n];
        area += a[0] * b[1] - a[1] * b[0];
    }
    Some((area / 2.0).abs())
}

/// `path + path`: concatenation. Only open paths can be concatenated; if either
/// operand is closed the result is NULL, matching PG.
pub fn path_concat(a: &PathVal, b: &PathVal) -> Option<PathVal> {
    if a.closed || b.closed {
        return None;
    }
    let mut pts = a.pts.clone();
    pts.extend_from_slice(&b.pts);
    Some(PathVal { closed: false, pts })
}

/// One of the checked point arithmetic operations (`point_add` and friends).
type PointOp = fn(&[f64; 2], &[f64; 2]) -> Result<[f64; 2], GeoError>;

/// Apply a per-vertex point operation, preserving open/closed. Backs
/// `path + point`, `- point`, `* point` and `/ point`.
fn path_map_pt(p: &PathVal, q: &[f64; 2], f: PointOp) -> Result<PathVal, GeoError> {
    let pts = p.pts.iter().map(|v| f(v, q)).collect::<Result<_, _>>()?;
    Ok(PathVal {
        closed: p.closed,
        pts,
    })
}

/// `path + point`: translate.
pub fn path_add_pt(p: &PathVal, q: &[f64; 2]) -> Result<PathVal, GeoError> {
    path_map_pt(p, q, point_add)
}
/// `path - point`: translate by the negated point.
pub fn path_sub_pt(p: &PathVal, q: &[f64; 2]) -> Result<PathVal, GeoError> {
    path_map_pt(p, q, point_sub)
}
/// `path * point`: rotate / scale (complex multiply each vertex).
pub fn path_mul_pt(p: &PathVal, q: &[f64; 2]) -> Result<PathVal, GeoError> {
    path_map_pt(p, q, point_mul)
}
/// `path / point`: rotate / scale (complex divide each vertex). Dividing by
/// `(0,0)` raises `division by zero`.
pub fn path_div_pt(p: &PathVal, q: &[f64; 2]) -> Result<PathVal, GeoError> {
    path_map_pt(p, q, point_div)
}

/// `path <-> path`: the shortest distance between any segment of one and any
/// segment of the other. Two paths with no segments at all (one-point open
/// paths) have no distance, so the result is NULL like PG's.
pub fn path_distance(a: &PathVal, b: &PathVal) -> Option<f64> {
    let mut best: Option<f64> = None;
    for s1 in a.segments() {
        for s2 in b.segments() {
            let d = dist_seg_seg(&s1, &s2);
            best = Some(best.map_or(d, |m: f64| m.min(d)));
        }
    }
    best
}

/// `path <-> point` (and `point <-> path`): the shortest distance from the point
/// to any segment. A path with no segments at all (a one-point *open* path) has
/// no candidate to measure against and PG reports `0` — note this differs from
/// `path <-> path`, which is NULL in the same situation.
pub fn dist_path_point(p: &PathVal, q: &[f64; 2]) -> f64 {
    p.segments()
        .map(|s| dist_point_seg(q, &s))
        .fold(None, |best: Option<f64>, d| {
            Some(best.map_or(d, |m| m.min(d)))
        })
        .unwrap_or(0.0)
}

/// Whether a point lies on the boundary of the path (any segment, including the
/// closing one).
fn point_on_path_boundary(q: &[f64; 2], p: &PathVal) -> bool {
    p.segments().any(|s| point_on_seg(q, &s))
}

/// Whether a point lies inside or on the boundary of the polygon formed by the
/// vertex list, by winding number. Self-intersecting outlines therefore follow
/// the nonzero rule, matching PG's crossing-count behavior.
fn point_inside(q: &[f64; 2], pts: &[[f64; 2]]) -> bool {
    let n = pts.len();
    // `is_left > 0` when `q` lies left of the directed edge a -> b.
    let is_left = |a: &[f64; 2], b: &[f64; 2]| {
        (b[0] - a[0]) * (q[1] - a[1]) - (q[0] - a[0]) * (b[1] - a[1])
    };
    let mut wn = 0i64;
    for i in 0..n {
        let a = &pts[i];
        let b = &pts[(i + 1) % n];
        if a[1] <= q[1] {
            if b[1] > q[1] && is_left(a, b) > 0.0 {
                wn += 1;
            }
        } else if b[1] <= q[1] && is_left(a, b) < 0.0 {
            wn -= 1;
        }
    }
    wn != 0
}

/// `point <@ path`: for an open path, the point lies on one of the segments;
/// for a closed path, PG treats the vertex list as a region, so it is an
/// inside-or-on-the-boundary test.
pub fn on_ppath(q: &[f64; 2], p: &PathVal) -> bool {
    if !p.closed {
        return point_on_path_boundary(q, p);
    }
    point_on_path_boundary(q, p) || point_inside(q, &p.pts)
}

/// `path @> point`. PG defines this as exactly the commutator of `point <@ path`
/// with no open/closed distinction, so an *open* path does contain the points
/// lying on its outline.
pub fn path_contain_pt(p: &PathVal, q: &[f64; 2]) -> bool {
    on_ppath(q, p)
}

/// `path ?# path`: any segment of one crosses any segment of the other.
pub fn path_inter(a: &PathVal, b: &PathVal) -> bool {
    a.segments()
        .any(|s1| b.segments().any(|s2| lseg_interpt(&s1, &s2).is_some()))
}

/// `= <> < <= > >=` on paths compare the *number of points* only — PG's b-tree
/// ordering for `path` is by vertex count, so `'[(0,0),(1,1)]' = '((5,5),(6,6))'`
/// is true.
pub fn path_n_cmp(a: &PathVal, b: &PathVal) -> std::cmp::Ordering {
    a.pts.len().cmp(&b.pts.len())
}

// ---------------------------------------------------------------------------
// box
// ---------------------------------------------------------------------------
//
// A `box` is stored normalized as `[high.x, high.y, low.x, low.y]` — PG swaps
// the corners on input so that `high` is componentwise >= `low`, which is why
// `'(2.0,2.0,0.0,0.0)'::box` prints as `(2,2),(0,0)`.

fn box_high(b: &[f64; 4]) -> [f64; 2] {
    [b[0], b[1]]
}
fn box_low(b: &[f64; 4]) -> [f64; 2] {
    [b[2], b[3]]
}

/// Put the two corners into PG's normal form (high componentwise >= low). The
/// swap is per coordinate, so `(0,2,2,0)` and `(2,0,0,2)` both normalize to
/// `(2,2),(0,0)`.
fn box_normalize(mut b: [f64; 4]) -> [f64; 4] {
    if b[0] < b[2] {
        b.swap(0, 2);
    }
    if b[1] < b[3] {
        b.swap(1, 3);
    }
    b
}

/// Parse a `box`: `(x1,y1,x2,y2)`, `((x1,y1),(x2,y2))`, `(x1,y1),(x2,y2)` and
/// the bare `x1,y1,x2,y2` form. Unlike `lseg` the bracketed `[...]` spelling is
/// rejected, and the corners are normalized on input.
pub fn parse_box(orig: &str) -> Result<[f64; 4], GeoError> {
    let (_open, pts) = path_decode(orig, false, 2, true, "box")?;
    Ok(box_normalize([
        pts[0][0], pts[0][1], pts[1][0], pts[1][1],
    ]))
}

/// Format a `box` as `(hx,hy),(lx,ly)` — note PG prints no grouping parens.
pub fn format_box(b: &[f64; 4], efd: i32) -> String {
    format!(
        "({},{}),({},{})",
        crate::float::fmt_f64(b[0], efd),
        crate::float::fmt_f64(b[1], efd),
        crate::float::fmt_f64(b[2], efd),
        crate::float::fmt_f64(b[3], efd)
    )
}

/// `box(p1, p2)` / `point::box` when both points are the same.
pub fn box_from_points(p1: &[f64; 2], p2: &[f64; 2]) -> [f64; 4] {
    box_normalize([p1[0], p1[1], p2[0], p2[1]])
}

/// `box(point)` / `point::box`: the degenerate box at that point.
pub fn box_from_point(p: &[f64; 2]) -> [f64; 4] {
    [p[0], p[1], p[0], p[1]]
}

/// `width(box)`.
pub fn box_width(b: &[f64; 4]) -> f64 {
    b[0] - b[2]
}
/// `height(box)`.
pub fn box_height(b: &[f64; 4]) -> f64 {
    b[1] - b[3]
}
/// `area(box)`.
pub fn box_area(b: &[f64; 4]) -> f64 {
    box_width(b) * box_height(b)
}
/// `@@ box` / `center(box)` / `box::point`.
pub fn box_center(b: &[f64; 4]) -> [f64; 2] {
    [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0]
}
/// `diagonal(box)` / `lseg(box)` / `box::lseg`: the high-to-low diagonal.
pub fn box_diagonal(b: &[f64; 4]) -> [f64; 4] {
    [b[0], b[1], b[2], b[3]]
}
/// `bound_box(b1, b2)`: the smallest box containing both.
pub fn bound_box(a: &[f64; 4], b: &[f64; 4]) -> [f64; 4] {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

/// `&&` overlap: the two boxes share at least a boundary point.
pub fn box_overlap(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_ge(a[0], b[2]) && fp_ge(b[0], a[2]) && fp_ge(a[1], b[3]) && fp_ge(b[1], a[3])
}
/// `<<` strictly left of.
pub fn box_left(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_lt(a[0], b[2])
}
/// `>>` strictly right of.
pub fn box_right(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_gt(a[2], b[0])
}
/// `&<` does not extend to the right of.
pub fn box_over_left(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_le(a[0], b[0])
}
/// `&>` does not extend to the left of.
pub fn box_over_right(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_ge(a[2], b[2])
}
/// `<<|` strictly below.
pub fn box_below(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_lt(a[1], b[3])
}
/// `|>>` strictly above.
pub fn box_above(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_gt(a[3], b[1])
}
/// `&<|` does not extend above.
pub fn box_over_below(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_le(a[1], b[1])
}
/// `|&>` does not extend below.
pub fn box_over_above(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_ge(a[3], b[3])
}
/// `<^` is below (touching allowed).
pub fn box_below_eq(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_le(a[1], b[3])
}
/// `>^` is above (touching allowed).
pub fn box_above_eq(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_ge(a[3], b[1])
}
/// `@>` contains.
pub fn box_contain(a: &[f64; 4], b: &[f64; 4]) -> bool {
    fp_ge(a[0], b[0]) && fp_le(a[2], b[2]) && fp_ge(a[1], b[1]) && fp_le(a[3], b[3])
}
/// `<@` contained in.
pub fn box_contained(a: &[f64; 4], b: &[f64; 4]) -> bool {
    box_contain(b, a)
}
/// `~=` same as: identical corners. (Plain `=` compares *area*, not identity.)
pub fn box_same(a: &[f64; 4], b: &[f64; 4]) -> bool {
    point_eq(&box_high(a), &box_high(b)) && point_eq(&box_low(a), &box_low(b))
}
/// `?#` the two boxes have at least one point in common (same test as `&&`).
pub fn box_intersects(a: &[f64; 4], b: &[f64; 4]) -> bool {
    box_overlap(a, b)
}
/// `#` the intersection box, or NULL when they do not overlap.
pub fn box_intersect(a: &[f64; 4], b: &[f64; 4]) -> Option<[f64; 4]> {
    if !box_overlap(a, b) {
        return None;
    }
    Some([
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[2].max(b[2]),
        a[3].max(b[3]),
    ])
}

/// `= <> < <= > >=` on boxes compare **area**, so `'(0,0,2,2)' = '(1,1,3,3)'`
/// is true. (Identity is `~=`.)
pub fn box_area_cmp(a: &[f64; 4], b: &[f64; 4]) -> std::cmp::Ordering {
    let (x, y) = (box_area(a), box_area(b));
    if fp_eq(x, y) {
        std::cmp::Ordering::Equal
    } else if x < y {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Greater
    }
}

/// `box <op> point` arithmetic: apply the point operation to both corners and
/// re-normalize (a rotation can swap which corner is "high").
fn box_map_pt(b: &[f64; 4], q: &[f64; 2], f: PointOp) -> Result<[f64; 4], GeoError> {
    let h = f(&box_high(b), q)?;
    let l = f(&box_low(b), q)?;
    Ok(box_normalize([h[0], h[1], l[0], l[1]]))
}
/// `box + point`.
pub fn box_add_pt(b: &[f64; 4], q: &[f64; 2]) -> Result<[f64; 4], GeoError> {
    box_map_pt(b, q, point_add)
}
/// `box - point`.
pub fn box_sub_pt(b: &[f64; 4], q: &[f64; 2]) -> Result<[f64; 4], GeoError> {
    box_map_pt(b, q, point_sub)
}
/// `box * point`.
pub fn box_mul_pt(b: &[f64; 4], q: &[f64; 2]) -> Result<[f64; 4], GeoError> {
    box_map_pt(b, q, point_mul)
}
/// `box / point`.
pub fn box_div_pt(b: &[f64; 4], q: &[f64; 2]) -> Result<[f64; 4], GeoError> {
    box_map_pt(b, q, point_div)
}

/// `box @> point` (and `point <@ box`): inside or on the boundary. This is one
/// of the few geometric tests PG makes **exactly** rather than fuzzily, so a
/// point a denormal step outside the box is outside it — `point
/// '(1e-300,-1e-300)' <@ box '(0,0,100,100)'` is false.
pub fn box_contain_pt(b: &[f64; 4], q: &[f64; 2]) -> bool {
    b[0] >= q[0] && b[2] <= q[0] && b[1] >= q[1] && b[3] <= q[1]
}

/// The four sides of a box, as segments, in the order PG walks them. The order
/// is observable: [`close_point_box`] keeps the first *fuzzily* nearest side, so
/// candidates whose distances differ by less than `EPSILON` are decided here.
fn box_edges(b: &[f64; 4]) -> [[f64; 4]; 4] {
    let (hx, hy, lx, ly) = (b[0], b[1], b[2], b[3]);
    [
        [lx, ly, lx, hy],
        [lx, hy, hx, hy],
        [hx, hy, hx, ly],
        [hx, ly, lx, ly],
    ]
}

/// `point ## box`: the point itself when it lies in the box, otherwise the
/// nearest point on the box's outline. Ties — including two candidates merely
/// *within `EPSILON`* of each other — go to the earlier side.
pub fn close_point_box(q: &[f64; 2], b: &[f64; 4]) -> [f64; 2] {
    if box_contain_pt(b, q) {
        return *q;
    }
    let mut best: Option<(f64, [f64; 2])> = None;
    for e in box_edges(b) {
        let c = close_point_seg(q, &e);
        let d = point_distance(q, &c);
        if best.is_none_or(|(bd, _)| fp_lt(d, bd)) {
            best = Some((d, c));
        }
    }
    best.map_or(*q, |(_, c)| c)
}

/// `point <-> box`: `0` inside the box, otherwise the distance to its outline.
pub fn dist_point_box(q: &[f64; 2], b: &[f64; 4]) -> f64 {
    if box_contain_pt(b, q) {
        return 0.0;
    }
    point_distance(q, &close_point_box(q, b))
}

/// `lseg <@ box`: both endpoints lie in the box (so the whole segment does).
pub fn lseg_inside_box(l: &[f64; 4], b: &[f64; 4]) -> bool {
    box_contain_pt(b, &lseg_p0(l)) && box_contain_pt(b, &lseg_p1(l))
}

/// `lseg ?# box`: the segment meets the box at all.
pub fn lseg_intersects_box(l: &[f64; 4], b: &[f64; 4]) -> bool {
    box_contain_pt(b, &lseg_p0(l))
        || box_contain_pt(b, &lseg_p1(l))
        || box_edges(b).iter().any(|e| lseg_interpt(l, e).is_some())
}

/// `lseg <-> box`: `0` when they meet, otherwise the nearest approach to the
/// box's outline.
pub fn dist_lseg_box(l: &[f64; 4], b: &[f64; 4]) -> f64 {
    if lseg_intersects_box(l, b) {
        return 0.0;
    }
    box_edges(b)
        .iter()
        .map(|e| dist_seg_seg(l, e))
        .fold(f64::INFINITY, f64::min)
}

/// The point on segment `a` nearest to segment `b`.
fn close_seg_on_first(a: &[f64; 4], b: &[f64; 4]) -> [f64; 2] {
    let cands = [
        close_point_seg(&lseg_p0(b), a),
        close_point_seg(&lseg_p1(b), a),
        lseg_p0(a),
        lseg_p1(a),
    ];
    let mut best = (f64::INFINITY, lseg_p0(a));
    for c in cands {
        let d = dist_point_seg(&c, b);
        if d < best.0 {
            best = (d, c);
        }
    }
    best.1
}

/// `lseg ## box`. When the segment meets the box, PG reports the point of the
/// *segment* nearest the box's center; otherwise the point of the box's outline
/// nearest the segment.
pub fn close_lseg_box(l: &[f64; 4], b: &[f64; 4]) -> [f64; 2] {
    if lseg_intersects_box(l, b) {
        return close_point_seg(&box_center(b), l);
    }
    let mut best = (f64::INFINITY, box_high(b));
    for e in box_edges(b) {
        let c = close_seg_on_first(&e, l);
        let d = dist_point_seg(&c, l);
        if d < best.0 {
            best = (d, c);
        }
    }
    best.1
}

/// `box <-> box`. PG measures this **center to center**, not outline to
/// outline, so two disjoint boxes never report `0`.
pub fn dist_box_box(a: &[f64; 4], b: &[f64; 4]) -> f64 {
    point_distance(&box_center(a), &box_center(b))
}

/// `box::circle`: the circumscribed circle (center, half the diagonal).
pub fn box_to_circle(b: &[f64; 4]) -> [f64; 3] {
    let c = box_center(b);
    [c[0], c[1], point_distance(&box_high(b), &c)]
}

/// `box::polygon`: the four corners, counterclockwise from the low corner.
pub fn box_to_polygon(b: &[f64; 4]) -> PolygonVal {
    let (hx, hy, lx, ly) = (b[0], b[1], b[2], b[3]);
    PolygonVal {
        pts: vec![[lx, ly], [lx, hy], [hx, hy], [hx, ly]],
    }
}

// ---------------------------------------------------------------------------
// line
// ---------------------------------------------------------------------------
//
// A `line` is the infinite line `Ax + By + C = 0`, stored as `[A, B, C]`.

const INVALID_PARAMETER_VALUE: &str = "22023";

fn line_spec_error(detail: &str) -> GeoError {
    GeoError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid line specification: {detail}"),
    }
}

/// Parse a `line`: the coefficient form `{A,B,C}`, or any of the two-point
/// spellings (`[(x1,y1),(x2,y2)]`, `(x1,y1),(x2,y2)`, `x1,y1,x2,y2`).
pub fn parse_line(orig: &str) -> Result<[f64; 3], GeoError> {
    let trimmed = orig.trim_start();
    if trimmed.starts_with('{') {
        let mut cur = Cur::new(orig);
        cur.skip_ws();
        cur.i += 1;
        let a = single_decode(&mut cur, "line", orig)?;
        cur.skip_ws();
        if !cur.eat(b',') {
            return Err(syntax("line", orig));
        }
        let b = single_decode(&mut cur, "line", orig)?;
        cur.skip_ws();
        if !cur.eat(b',') {
            return Err(syntax("line", orig));
        }
        let c = single_decode(&mut cur, "line", orig)?;
        cur.skip_ws();
        if !cur.eat(b'}') {
            return Err(syntax("line", orig));
        }
        cur.skip_ws();
        if !cur.at_end() {
            return Err(syntax("line", orig));
        }
        if fp_eq(a, 0.0) && fp_eq(b, 0.0) {
            return Err(line_spec_error("A and B cannot both be zero"));
        }
        return Ok([a, b, c]);
    }
    let (_open, pts) = path_decode(orig, true, 2, true, "line")?;
    line_from_points(&pts[0], &pts[1])
}

/// Format a `line` as `{A,B,C}`.
pub fn format_line(l: &[f64; 3], efd: i32) -> String {
    format!(
        "{{{},{},{}}}",
        crate::float::fmt_f64(l[0], efd),
        crate::float::fmt_f64(l[1], efd),
        crate::float::fmt_f64(l[2], efd)
    )
}

/// `line(p1, p2)`: the line through two distinct points. PG normalizes to
/// `{-1,0,x}` when vertical and `{0,-1,y}` when horizontal, and otherwise to
/// `{slope,-1,intercept}`.
pub fn line_from_points(p1: &[f64; 2], p2: &[f64; 2]) -> Result<[f64; 3], GeoError> {
    if point_eq(p1, p2) {
        return Err(line_spec_error("must be two distinct points"));
    }
    if fp_eq(p1[0], p2[0]) {
        return Ok([-1.0, 0.0, p1[0]]);
    }
    if fp_eq(p1[1], p2[1]) {
        return Ok([0.0, -1.0, p1[1]]);
    }
    let a = (p2[1] - p1[1]) / (p2[0] - p1[0]);
    Ok([a, -1.0, p1[1] - a * p1[0]])
}

/// `=` on lines is scale invariant (`{1,2,3}` equals `{2,4,6}` and
/// `{-1,-2,-3}`). When any coefficient is NaN, PG falls back to insisting on
/// exact equality with `NaN = NaN`, so `{nan,1,nan}` equals only itself.
pub fn line_eq(a: &[f64; 3], b: &[f64; 3]) -> bool {
    if a.iter().chain(b.iter()).any(|v| v.is_nan()) {
        return a
            .iter()
            .zip(b.iter())
            .all(|(x, y)| x == y || (x.is_nan() && y.is_nan()));
    }
    let ratio = if !fp_eq(b[0], 0.0) {
        a[0] / b[0]
    } else if !fp_eq(b[1], 0.0) {
        a[1] / b[1]
    } else if !fp_eq(b[2], 0.0) {
        a[2] / b[2]
    } else {
        1.0
    };
    fp_eq(a[0], ratio * b[0]) && fp_eq(a[1], ratio * b[1]) && fp_eq(a[2], ratio * b[2])
}

/// The `(A,B)` normal vector's length; `0` only for a degenerate line, which
/// input validation rejects.
fn line_norm(l: &[f64; 3]) -> f64 {
    hypot(l[0], l[1])
}

/// `?- line` horizontal: the `x` coefficient vanishes.
pub fn line_horizontal(l: &[f64; 3]) -> bool {
    fp_eq(l[0] / line_norm(l), 0.0)
}
/// `?| line` vertical: the `y` coefficient vanishes.
pub fn line_vertical(l: &[f64; 3]) -> bool {
    fp_eq(l[1] / line_norm(l), 0.0)
}

/// `?||` parallel. Compared through the normalized cross product so the test is
/// independent of how the coefficients are scaled.
pub fn line_parallel(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_eq((a[0] * b[1] - b[0] * a[1]) / (line_norm(a) * line_norm(b)), 0.0)
}
/// `?-|` perpendicular, through the normalized dot product.
pub fn line_perpendicular(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_eq((a[0] * b[0] + a[1] * b[1]) / (line_norm(a) * line_norm(b)), 0.0)
}

/// `#` the intersection point of two lines; NULL when they are parallel (which
/// includes two spellings of the same line).
pub fn line_interpt(a: &[f64; 3], b: &[f64; 3]) -> Option<[f64; 2]> {
    if line_parallel(a, b) {
        return None;
    }
    let det = a[0] * b[1] - b[0] * a[1];
    Some([
        (a[1] * b[2] - b[1] * a[2]) / det,
        (b[0] * a[2] - a[0] * b[2]) / det,
    ])
}

/// `?#` the two lines meet in exactly one point.
pub fn line_intersects(a: &[f64; 3], b: &[f64; 3]) -> bool {
    !line_parallel(a, b)
}

/// `point <-> line` (and `line <-> point`). Measured as the distance to the
/// closest point rather than by `|Ax+By+C|/hypot(A,B)`: PG does it that way, and
/// the two differ in the last ulp on ordinary input and in their NaN/Infinity
/// results at the extremes.
pub fn dist_point_line(q: &[f64; 2], l: &[f64; 3]) -> f64 {
    point_distance(q, &close_point_line(q, l))
}

/// `point ## line`: the foot of the perpendicular from the point. Vertical and
/// horizontal lines are special-cased (as in PG) so the coordinate the line
/// pins comes out exactly; otherwise this intersects the line with the
/// perpendicular through the point.
pub fn close_point_line(q: &[f64; 2], l: &[f64; 3]) -> [f64; 2] {
    let (a, b, c) = (l[0], l[1], l[2]);
    if fp_eq(b, 0.0) {
        return [c / -a, q[1]];
    }
    if fp_eq(a, 0.0) {
        return [q[0], c / -b];
    }
    let invm = -1.0 / a;
    let perp = [invm, -1.0, q[1] - invm * q[0]];
    line_interpt(&perp, l).unwrap_or([f64::NAN, f64::NAN])
}

/// `point <@ line`: the point lies on the line.
pub fn point_on_line(q: &[f64; 2], l: &[f64; 3]) -> bool {
    fp_eq(dist_point_line(q, l), 0.0)
}

/// `line <-> line`: `0` unless they are parallel, in which case it is the
/// constant separation.
pub fn dist_line_line(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    if !line_parallel(a, b) {
        return 0.0;
    }
    // Any point of `a` will do; pick the one on whichever axis `a` crosses.
    let p = if !fp_eq(a[1], 0.0) {
        [0.0, -a[2] / a[1]]
    } else {
        [-a[2] / a[0], 0.0]
    };
    dist_point_line(&p, b)
}

/// The line carrying a segment. `None` for a degenerate (zero-length) segment.
fn lseg_to_line(l: &[f64; 4]) -> Option<[f64; 3]> {
    line_from_points(&lseg_p0(l), &lseg_p1(l)).ok()
}

/// `lseg <@ line`: both endpoints lie on the line.
pub fn lseg_on_line(s: &[f64; 4], l: &[f64; 3]) -> bool {
    point_on_line(&lseg_p0(s), l) && point_on_line(&lseg_p1(s), l)
}

/// `lseg ?# line`: the segment touches or crosses the line.
pub fn lseg_intersects_line(s: &[f64; 4], l: &[f64; 3]) -> bool {
    let norm = line_norm(l);
    let f0 = (l[0] * s[0] + l[1] * s[1] + l[2]) / norm;
    let f1 = (l[0] * s[2] + l[1] * s[3] + l[2]) / norm;
    fp_eq(f0, 0.0) || fp_eq(f1, 0.0) || (f0 < 0.0) != (f1 < 0.0)
}

/// `lseg <-> line`: `0` when they meet, otherwise the nearer endpoint's
/// distance.
pub fn dist_lseg_line(s: &[f64; 4], l: &[f64; 3]) -> f64 {
    if lseg_intersects_line(s, l) {
        return 0.0;
    }
    dist_point_line(&lseg_p0(s), l).min(dist_point_line(&lseg_p1(s), l))
}

/// `line ## lseg`: the point of the *segment* nearest the line. NULL when the
/// segment runs parallel to the line (including lying on it), matching PG.
pub fn close_line_lseg(l: &[f64; 3], s: &[f64; 4]) -> Option<[f64; 2]> {
    let sl = lseg_to_line(s)?;
    if line_parallel(l, &sl) {
        return None;
    }
    if lseg_intersects_line(s, l) {
        return line_interpt(l, &sl);
    }
    let (p0, p1) = (lseg_p0(s), lseg_p1(s));
    Some(if dist_point_line(&p0, l) <= dist_point_line(&p1, l) {
        p0
    } else {
        p1
    })
}

/// `line ?# box`: the line passes through the box. True unless all four corners
/// sit strictly on the same side of it.
pub fn line_intersects_box(l: &[f64; 3], b: &[f64; 4]) -> bool {
    let corners = [
        [b[0], b[1]],
        [b[0], b[3]],
        [b[2], b[1]],
        [b[2], b[3]],
    ];
    let mut neg = false;
    let mut pos = false;
    for c in corners {
        let f = (l[0] * c[0] + l[1] * c[1] + l[2]) / line_norm(l);
        if fp_eq(f, 0.0) {
            return true;
        }
        if f < 0.0 {
            neg = true;
        } else {
            pos = true;
        }
    }
    neg && pos
}

// ---------------------------------------------------------------------------
// circle
// ---------------------------------------------------------------------------
//
// A `circle` is stored as `[center.x, center.y, radius]`.

fn circle_c(c: &[f64; 3]) -> [f64; 2] {
    [c[0], c[1]]
}

/// Parse a `circle`: `<(x,y),r>`, `((x,y),r)`, `(x,y),r` and the bare `x,y,r`.
/// A negative radius is a syntax error, matching PG.
pub fn parse_circle(orig: &str) -> Result<[f64; 3], GeoError> {
    let mut cur = Cur::new(orig);
    cur.skip_ws();
    let angled = cur.eat(b'<');
    // A leading `(` immediately followed by another `(` groups the whole value
    // (`((1,2),3)`); otherwise it belongs to the center point itself.
    cur.skip_ws();
    let mut grouped = false;
    if cur.peek() == Some(b'(') {
        let b = orig.as_bytes();
        let mut j = cur.i + 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == b'(' {
            grouped = true;
            cur.i += 1;
        }
    }
    let center = pair_decode(&mut cur, "circle", orig)?;
    cur.skip_ws();
    if !cur.eat(b',') {
        return Err(syntax("circle", orig));
    }
    let radius = single_decode(&mut cur, "circle", orig)?;
    if grouped {
        cur.skip_ws();
        if !cur.eat(b')') {
            return Err(syntax("circle", orig));
        }
    }
    if angled {
        cur.skip_ws();
        if !cur.eat(b'>') {
            return Err(syntax("circle", orig));
        }
    }
    cur.skip_ws();
    if !cur.at_end() || radius < 0.0 {
        return Err(syntax("circle", orig));
    }
    Ok([center[0], center[1], radius])
}

/// Format a `circle` as `<(x,y),r>`.
pub fn format_circle(c: &[f64; 3], efd: i32) -> String {
    format!(
        "<({},{}),{}>",
        crate::float::fmt_f64(c[0], efd),
        crate::float::fmt_f64(c[1], efd),
        crate::float::fmt_f64(c[2], efd)
    )
}

/// `circle(point, radius)`.
pub fn circle_from_point_radius(p: &[f64; 2], r: f64) -> [f64; 3] {
    [p[0], p[1], r]
}
/// `@@ circle` / `center(circle)` / `point(circle)` / `circle::point`.
pub fn circle_center(c: &[f64; 3]) -> [f64; 2] {
    circle_c(c)
}
/// `radius(circle)`.
pub fn circle_radius(c: &[f64; 3]) -> f64 {
    c[2]
}
/// `diameter(circle)`.
pub fn circle_diameter(c: &[f64; 3]) -> f64 {
    2.0 * c[2]
}
/// `area(circle)`.
pub fn circle_area(c: &[f64; 3]) -> f64 {
    std::f64::consts::PI * c[2] * c[2]
}

/// `circle::box` / `box(circle)`: the inscribed box.
pub fn circle_to_box(c: &[f64; 3]) -> [f64; 4] {
    let half = c[2] / std::f64::consts::SQRT_2;
    [c[0] + half, c[1] + half, c[0] - half, c[1] - half]
}
/// `circle(box)`: the circumscribed circle of the box.
pub fn circle_from_box(b: &[f64; 4]) -> [f64; 3] {
    box_to_circle(b)
}

/// The vertex count `circle::polygon` uses when none is given.
pub const CIRCLE_POLYGON_NPTS: i32 = 12;

/// `polygon(npts, circle)` / `circle::polygon`: `npts` points evenly spaced
/// around the circle, starting at the leftmost one and running clockwise, which
/// is the ordering PG's output shows.
pub fn circle_to_polygon(npts: i32, c: &[f64; 3]) -> Result<PolygonVal, GeoError> {
    if npts < 2 {
        return Err(GeoError {
            sqlstate: INVALID_PARAMETER_VALUE,
            message: format!("must request at least 2 points, got {npts}"),
        });
    }
    let n = usize::try_from(npts).unwrap_or(0);
    let step = 2.0 * std::f64::consts::PI / f64::from(npts);
    let pts = (0..n)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let angle = step * i as f64;
            [c[0] - c[2] * angle.cos(), c[1] + c[2] * angle.sin()]
        })
        .collect();
    Ok(PolygonVal { pts })
}

/// `circle(polygon)`: centered on the vertex centroid, with the mean vertex
/// distance as the radius.
pub fn circle_from_polygon(p: &PolygonVal) -> [f64; 3] {
    let c = poly_center(p);
    #[allow(clippy::cast_precision_loss)]
    let n = p.pts.len() as f64;
    let r = p.pts.iter().fold(0.0, |acc, v| acc + point_distance(v, &c)) / n;
    [c[0], c[1], r]
}

/// `~=` same as: equal centers and equal radii.
pub fn circle_same(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_eq(a[2], b[2]) && point_eq(&circle_c(a), &circle_c(b))
}
/// `&&` overlap.
pub fn circle_overlap(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_le(point_distance(&circle_c(a), &circle_c(b)), a[2] + b[2])
}
/// `<<` strictly left of.
pub fn circle_left(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_lt(a[0] + a[2], b[0] - b[2])
}
/// `>>` strictly right of.
pub fn circle_right(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_gt(a[0] - a[2], b[0] + b[2])
}
/// `&<` does not extend to the right of.
pub fn circle_over_left(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_le(a[0] + a[2], b[0] + b[2])
}
/// `&>` does not extend to the left of.
pub fn circle_over_right(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_ge(a[0] - a[2], b[0] - b[2])
}
/// `<<|` strictly below.
pub fn circle_below(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_lt(a[1] + a[2], b[1] - b[2])
}
/// `|>>` strictly above.
pub fn circle_above(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_gt(a[1] - a[2], b[1] + b[2])
}
/// `&<|` does not extend above.
pub fn circle_over_below(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_le(a[1] + a[2], b[1] + b[2])
}
/// `|&>` does not extend below.
pub fn circle_over_above(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_ge(a[1] - a[2], b[1] - b[2])
}
/// `@>` contains the other circle.
pub fn circle_contain(a: &[f64; 3], b: &[f64; 3]) -> bool {
    fp_le(point_distance(&circle_c(a), &circle_c(b)) + b[2], a[2])
}
/// `<@` contained in the other circle.
pub fn circle_contained(a: &[f64; 3], b: &[f64; 3]) -> bool {
    circle_contain(b, a)
}
/// `circle @> point` (and `point <@ circle`) / `pt_contained_circle`.
pub fn circle_contain_pt(c: &[f64; 3], q: &[f64; 2]) -> bool {
    fp_le(point_distance(&circle_c(c), q), c[2])
}

/// `= <> < <= > >=` on circles compare **area** (identity is `~=`).
pub fn circle_area_cmp(a: &[f64; 3], b: &[f64; 3]) -> std::cmp::Ordering {
    let (x, y) = (circle_area(a), circle_area(b));
    if fp_eq(x, y) {
        std::cmp::Ordering::Equal
    } else if x < y {
        std::cmp::Ordering::Less
    } else {
        std::cmp::Ordering::Greater
    }
}

/// `circle <-> circle`: the gap between the outlines, `0` when they overlap.
pub fn dist_circle_circle(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    (point_distance(&circle_c(a), &circle_c(b)) - a[2] - b[2]).max(0.0)
}
/// `point <-> circle`: `0` inside, otherwise the gap to the outline.
pub fn dist_point_circle(q: &[f64; 2], c: &[f64; 3]) -> f64 {
    (point_distance(q, &circle_c(c)) - c[2]).max(0.0)
}

/// `circle <op> point` translation: move the center, leaving the radius alone.
/// (`*` and `/` also scale the radius, so they do not go through here.)
fn circle_translate(c: &[f64; 3], q: &[f64; 2], f: PointOp) -> Result<[f64; 3], GeoError> {
    let center = f(&circle_c(c), q)?;
    Ok([center[0], center[1], c[2]])
}
/// `circle + point`.
pub fn circle_add_pt(c: &[f64; 3], q: &[f64; 2]) -> Result<[f64; 3], GeoError> {
    circle_translate(c, q, point_add)
}
/// `circle - point`.
pub fn circle_sub_pt(c: &[f64; 3], q: &[f64; 2]) -> Result<[f64; 3], GeoError> {
    circle_translate(c, q, point_sub)
}
/// `circle * point`: rotate/scale; the radius grows by `|point|`.
pub fn circle_mul_pt(c: &[f64; 3], q: &[f64; 2]) -> Result<[f64; 3], GeoError> {
    let center = point_mul(&circle_c(c), q)?;
    Ok([center[0], center[1], f8_mul(c[2], hypot(q[0], q[1]))?])
}
/// `circle / point`: rotate/scale; the radius shrinks by `|point|`.
pub fn circle_div_pt(c: &[f64; 3], q: &[f64; 2]) -> Result<[f64; 3], GeoError> {
    let center = point_div(&circle_c(c), q)?;
    Ok([center[0], center[1], f8_div(c[2], hypot(q[0], q[1]))?])
}

// ---------------------------------------------------------------------------
// polygon
// ---------------------------------------------------------------------------

/// A `polygon`: a non-empty list of vertices, rendered `((x,y),...)`. Unlike a
/// closed `path` it carries a bounding box in PG; here that is derived on
/// demand by [`poly_bbox`].
#[derive(deepsize::DeepSizeOf, Clone, Debug, PartialEq)]
pub struct PolygonVal {
    /// The vertices, in order. Always at least one.
    pub pts: Vec<[f64; 2]>,
}

impl PolygonVal {
    /// The closing outline of the polygon, one segment per vertex.
    fn edges(&self) -> impl Iterator<Item = [f64; 4]> + '_ {
        let n = self.pts.len();
        (0..n).map(move |i| {
            let a = self.pts[i];
            let b = self.pts[(i + 1) % n];
            [a[0], a[1], b[0], b[1]]
        })
    }
}

/// Parse a `polygon`: `((x1,y1),...)` and the unbracketed spellings. The
/// open-path `[...]` form is rejected.
pub fn parse_polygon(orig: &str) -> Result<PolygonVal, GeoError> {
    let npts = pair_count(orig);
    if npts == 0 {
        return Err(syntax("polygon", orig));
    }
    let (_open, pts) = path_decode(orig, false, npts, false, "polygon")?;
    Ok(PolygonVal { pts })
}

/// Format a `polygon` as `((x,y),...)`.
pub fn format_polygon(p: &PolygonVal, efd: i32) -> String {
    let mut out = String::from("(");
    for (i, pt) in p.pts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format_point(pt, efd));
    }
    out.push(')');
    out
}

/// `# polygon` / `npoints(polygon)`.
pub fn poly_npoints(p: &PolygonVal) -> i32 {
    i32::try_from(p.pts.len()).unwrap_or(i32::MAX)
}

/// `@@ polygon` / `point(polygon)` / `polygon::point`: the vertex centroid.
pub fn poly_center(p: &PolygonVal) -> [f64; 2] {
    #[allow(clippy::cast_precision_loss)]
    let n = p.pts.len() as f64;
    let (sx, sy) = p
        .pts
        .iter()
        .fold((0.0, 0.0), |(x, y), v| (x + v[0], y + v[1]));
    [sx / n, sy / n]
}

/// `polygon::box` / `box(polygon)`: the bounding box.
pub fn poly_bbox(p: &PolygonVal) -> [f64; 4] {
    let mut b = [
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
        f64::INFINITY,
        f64::INFINITY,
    ];
    for v in &p.pts {
        b[0] = b[0].max(v[0]);
        b[1] = b[1].max(v[1]);
        b[2] = b[2].min(v[0]);
        b[3] = b[3].min(v[1]);
    }
    b
}

/// `polygon::path` / `path(polygon)`: the same vertices as a *closed* path.
pub fn poly_to_path(p: &PolygonVal) -> PathVal {
    PathVal {
        closed: true,
        pts: p.pts.clone(),
    }
}

/// `path::polygon` / `polygon(path)`. An open path has no interior, so PG
/// refuses the conversion with `22023`.
pub fn path_to_polygon(p: &PathVal) -> Result<PolygonVal, GeoError> {
    if !p.closed {
        return Err(GeoError {
            sqlstate: INVALID_PARAMETER_VALUE,
            message: "open path cannot be converted to polygon".to_string(),
        });
    }
    Ok(PolygonVal {
        pts: p.pts.clone(),
    })
}

/// `polygon(box)` / `box::polygon`.
pub fn poly_from_box(b: &[f64; 4]) -> PolygonVal {
    box_to_polygon(b)
}

/// `~=` same as: the same vertices in the same order.
pub fn poly_same(a: &PolygonVal, b: &PolygonVal) -> bool {
    a.pts.len() == b.pts.len() && a.pts.iter().zip(b.pts.iter()).all(|(x, y)| point_eq(x, y))
}

/// `polygon @> point` (and `point <@ polygon`) / `pt_contained_poly`: inside or
/// on the outline.
pub fn poly_contain_pt(p: &PolygonVal, q: &[f64; 2]) -> bool {
    p.edges().any(|e| point_on_seg(q, &e)) || point_inside(q, &p.pts)
}

/// `&&` overlap: bounding boxes touch, and then either an edge pair crosses or
/// one polygon's first vertex lies inside the other.
pub fn poly_overlap(a: &PolygonVal, b: &PolygonVal) -> bool {
    if !box_overlap(&poly_bbox(a), &poly_bbox(b)) {
        return false;
    }
    if a
        .edges()
        .any(|e1| b.edges().any(|e2| lseg_interpt(&e1, &e2).is_some()))
    {
        return true;
    }
    poly_contain_pt(a, &b.pts[0]) || poly_contain_pt(b, &a.pts[0])
}

/// `@>` contains: bounding box containment, every vertex of `b` inside `a`, and
/// no vertex of `a` strictly inside `b`.
pub fn poly_contain(a: &PolygonVal, b: &PolygonVal) -> bool {
    if !box_contain(&poly_bbox(a), &poly_bbox(b)) {
        return false;
    }
    if !b.pts.iter().all(|q| poly_contain_pt(a, q)) {
        return false;
    }
    !a.pts
        .iter()
        .any(|q| point_inside(q, &b.pts) && !b.edges().any(|e| point_on_seg(q, &e)))
}
/// `<@` contained in.
pub fn poly_contained(a: &PolygonVal, b: &PolygonVal) -> bool {
    poly_contain(b, a)
}

/// The positional operators compare bounding boxes, as PG's do.
/// `<<` strictly left of.
pub fn poly_left(a: &PolygonVal, b: &PolygonVal) -> bool {
    box_left(&poly_bbox(a), &poly_bbox(b))
}
/// `>>` strictly right of.
pub fn poly_right(a: &PolygonVal, b: &PolygonVal) -> bool {
    box_right(&poly_bbox(a), &poly_bbox(b))
}
/// `&<` does not extend to the right of.
pub fn poly_over_left(a: &PolygonVal, b: &PolygonVal) -> bool {
    box_over_left(&poly_bbox(a), &poly_bbox(b))
}
/// `&>` does not extend to the left of.
pub fn poly_over_right(a: &PolygonVal, b: &PolygonVal) -> bool {
    box_over_right(&poly_bbox(a), &poly_bbox(b))
}
/// `<<|` strictly below.
pub fn poly_below(a: &PolygonVal, b: &PolygonVal) -> bool {
    box_below(&poly_bbox(a), &poly_bbox(b))
}
/// `|>>` strictly above.
pub fn poly_above(a: &PolygonVal, b: &PolygonVal) -> bool {
    box_above(&poly_bbox(a), &poly_bbox(b))
}
/// `&<|` does not extend above.
pub fn poly_over_below(a: &PolygonVal, b: &PolygonVal) -> bool {
    box_over_below(&poly_bbox(a), &poly_bbox(b))
}
/// `|&>` does not extend below.
pub fn poly_over_above(a: &PolygonVal, b: &PolygonVal) -> bool {
    box_over_above(&poly_bbox(a), &poly_bbox(b))
}

/// `polygon <-> point` (and `point <-> polygon`): `0` inside, otherwise the
/// distance to the nearest edge.
pub fn dist_poly_point(p: &PolygonVal, q: &[f64; 2]) -> f64 {
    if poly_contain_pt(p, q) {
        return 0.0;
    }
    p.edges()
        .map(|e| dist_point_seg(q, &e))
        .fold(f64::INFINITY, f64::min)
}

/// `polygon <-> polygon`: `0` when they overlap, otherwise the nearest approach
/// between their outlines.
pub fn dist_poly_poly(a: &PolygonVal, b: &PolygonVal) -> f64 {
    if poly_overlap(a, b) {
        return 0.0;
    }
    a.edges()
        .map(|e1| {
            b.edges()
                .map(|e2| dist_seg_seg(&e1, &e2))
                .fold(f64::INFINITY, f64::min)
        })
        .fold(f64::INFINITY, f64::min)
}

/// `polygon <-> circle` (and `circle <-> polygon`): the polygon's distance to
/// the circle's center, less its radius.
pub fn dist_poly_circle(p: &PolygonVal, c: &[f64; 3]) -> f64 {
    (dist_poly_point(p, &circle_c(c)) - c[2]).max(0.0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn point_roundtrip_and_forms() -> anyhow::Result<()> {
        assert_eq!(parse_point("(1,2)")?, [1.0, 2.0]);
        assert_eq!(parse_point("1,2")?, [1.0, 2.0]);
        assert_eq!(parse_point(" ( -3.0 , 4.0 ) ")?, [-3.0, 4.0]);
        assert_eq!(format_point(&[5.1, 34.5], 0), "(5.1,34.5)");
        assert_eq!(format_point(&[0.0, 0.0], 0), "(0,0)");

        Ok(())
    }

    #[test]
    fn point_bad_input() {
        assert_eq!(parse_point("asdfasdf").unwrap_err().sqlstate, "22P02");
        assert_eq!(parse_point("(10.0 10.0)").unwrap_err().sqlstate, "22P02");
        assert_eq!(parse_point("(10.0, 10.0) x").unwrap_err().sqlstate, "22P02");
        assert_eq!(parse_point("(10.0,10.0").unwrap_err().sqlstate, "22P02");
        // Out of range keeps float8in's "double precision" message + 22003.
        let e = parse_point("(10.0, 1e+500)").unwrap_err();
        assert_eq!(e.sqlstate, "22003");
        assert!(e.message.contains("double precision"), "{}", e.message);
    }

    #[test]
    fn lseg_forms_and_format() -> anyhow::Result<()> {
        assert_eq!(parse_lseg("[(1,2),(3,4)]")?, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(parse_lseg("(0,0),(6,6)")?, [0.0, 0.0, 6.0, 6.0]);
        assert_eq!(parse_lseg("10,-10 ,-3,-4")?, [10.0, -10.0, -3.0, -4.0]);
        assert_eq!(
            parse_lseg("[-1e6,2e2,3e5, -4e1]")?,
            [-1_000_000.0, 200.0, 300_000.0, -40.0]
        );
        assert_eq!(parse_lseg("((0,0),(1,0))")?, [0.0, 0.0, 1.0, 0.0]);
        // A bare coordinate list inside a single pair of parens: the leading
        // '(' is a grouping paren because it is the last '(' in the string.
        assert_eq!(parse_lseg("(1,2,3,4)")?, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(parse_lseg("( 11,12,13,14) ")?, [11.0, 12.0, 13.0, 14.0]);
        assert_eq!(format_lseg(&[1.0, 2.0, 3.0, 4.0], 0), "[(1,2),(3,4)]");

        Ok(())
    }

    #[test]
    fn lseg_bad_input() {
        for bad in [
            "(3asdf,2 ,3,4r2)",
            "[1,2,3, 4",
            "[(,2),(3,4)]",
            "[(1,2),(3,4)",
            "(1,2)",
        ] {
            assert_eq!(parse_lseg(bad).unwrap_err().sqlstate, "22P02", "{bad}");
        }
    }

    #[test]
    fn point_ops() -> anyhow::Result<()> {
        assert_eq!(point_distance(&[0.0, 0.0], &[3.0, 4.0]), 5.0);
        assert!(point_left(&[-10.0, 0.0], &[0.0, 0.0]));
        assert!(point_eq(&[5.1, 34.5], &[5.1, 34.5]));
        assert!(point_eq(&[0.0, 0.0], &[0.000_000_9, 0.000_000_9]));
        assert!(!point_eq(&[0.0, 0.0], &[0.000_001_8, 0.000_001_8]));
        assert_eq!(point_mul(&[5.1, 34.5], &[-10.0, 0.0])?, [-51.0, -345.0]);
        // Underflow: 1e-300 * 1e-300 underflows to 0 from nonzero inputs.
        assert_eq!(
            point_mul(&[1e-300, -1e-300], &[1e-300, -1e-300])
                .unwrap_err()
                .sqlstate,
            "22003"
        );
        assert_eq!(point_div(&[5.1, 34.5], &[5.1, 34.5])?, [1.0, 0.0]);
        assert_eq!(
            point_div(&[1.0, 1.0], &[0.0, 0.0]).unwrap_err().sqlstate,
            "22012"
        );

        Ok(())
    }

    #[test]
    fn point_slope_cases() {
        assert_eq!(point_slope(&[0.0, 0.0], &[2.0, 1.0]), 0.5);
        // Vertical and coincident pairs both yield Infinity (PG's slope()).
        assert_eq!(point_slope(&[1.0, 2.0], &[1.0, 9.0]), f64::INFINITY);
        assert_eq!(point_slope(&[1.0, 1.0], &[1.0, 1.0]), f64::INFINITY);
        // A fuzzily-horizontal pair is exactly 0, not a tiny nonzero slope.
        assert_eq!(point_slope(&[0.0, 0.0], &[1_000_000.0, 0.000_000_5]), 0.0);
    }

    #[test]
    fn lseg_ops() {
        assert_eq!(lseg_center(&[1.0, 2.0, 3.0, 4.0]), [2.0, 3.0]);
        assert_eq!(lseg_length(&[0.0, 0.0, 3.0, 4.0]), 5.0);
        assert!(lseg_vertical(&[-10.0, 2.0, -10.0, 3.0]));
        assert!(lseg_horizontal(&[0.0, -20.0, 30.0, -20.0]));
        assert!(lseg_lt(&[0.0, 0.0, 2.0, 0.0], &[0.0, 0.0, 3.0, 0.0]));
        assert!(lseg_eq(&[0.0, 0.0, 2.0, 0.0], &[0.0, 0.0, 2.0, 0.0]));
        assert!(!lseg_eq(&[0.0, 0.0, 2.0, 0.0], &[0.0, 0.0, 0.0, 2.0]));
        assert_eq!(dist_point_seg(&[0.0, 1.0], &[0.0, 0.0, 1.0, 0.0]), 1.0);
        assert_eq!(
            dist_seg_seg(&[0.0, 0.0, 1.0, 0.0], &[0.0, 2.0, 1.0, 2.0]),
            2.0
        );
        assert_eq!(
            lseg_interpt(&[0.0, 0.0, 2.0, 0.0], &[1.0, -1.0, 1.0, 1.0]),
            Some([1.0, 0.0])
        );
        assert_eq!(
            lseg_interpt(&[0.0, 0.0, 2.0, 0.0], &[0.0, 1.0, 2.0, 1.0]),
            None
        );
        assert_eq!(
            close_point_seg(&[0.0, 5.0], &[0.0, 0.0, 10.0, 0.0]),
            [0.0, 0.0]
        );
        // `##` is NULL (None) for parallel segments, a point otherwise.
        assert_eq!(
            close_seg_seg(&[0.0, 0.0, 1.0, 0.0], &[0.0, 2.0, 1.0, 2.0]),
            None
        );
        assert_eq!(
            close_seg_seg(&[0.0, 0.0, 2.0, 0.0], &[1.0, 1.0, 1.0, 3.0]),
            Some([1.0, 1.0])
        );
    }

    #[test]
    fn lseg_parallel_perpendicular_are_scale_invariant() {
        // Slope-based comparison stays correct at large coordinate magnitudes,
        // where an absolute cross/dot product would drift past EPSILON.
        assert!(lseg_parallel(
            &[0.0, 0.0, 1e6, 1.0],
            &[0.0, 0.0, 1e6, 1.000_000_1]
        ));
        assert!(lseg_perpendicular(
            &[0.0, 0.0, 1000.0, 1.0],
            &[0.0, 0.0, 1.0, -999.999_5]
        ));
        // Two vertical segments are parallel; vertical ⟂ horizontal.
        assert!(lseg_parallel(&[0.0, 0.0, 0.0, 5.0], &[3.0, 0.0, 3.0, 10.0]));
        assert!(lseg_perpendicular(
            &[0.0, 0.0, 0.0, 5.0],
            &[0.0, 0.0, 5.0, 0.0]
        ));
    }

    /// Round-trip a path literal through parse + format at `extra_float_digits`
    /// 0, which is how the type renders on the wire by default.
    fn path_io(s: &str) -> Result<String, GeoError> {
        Ok(format_path(&parse_path(s)?, 0))
    }

    #[test]
    fn path_forms_and_format() -> anyhow::Result<()> {
        // Every spelling upstream's path.sql feeds the type.
        assert_eq!(path_io("[(1,2),(3,4)]")?, "[(1,2),(3,4)]");
        assert_eq!(path_io(" ( ( 1 , 2 ) , ( 3 , 4 ) ) ")?, "((1,2),(3,4))");
        assert_eq!(
            path_io("[ (0,0),(3,0),(4,5),(1,6) ]")?,
            "[(0,0),(3,0),(4,5),(1,6)]"
        );
        assert_eq!(path_io("((1,2) ,(3,4 ))")?, "((1,2),(3,4))");
        assert_eq!(path_io("1,2 ,3,4 ")?, "((1,2),(3,4))");
        assert_eq!(path_io(" [1,2,3, 4] ")?, "[(1,2),(3,4)]");
        assert_eq!(path_io("((10,20))")?, "((10,20))");
        assert_eq!(path_io("[ 11,12,13,14 ]")?, "[(11,12),(13,14)]");
        assert_eq!(path_io("( 11,12,13,14) ")?, "((11,12),(13,14))");

        Ok(())
    }

    /// A trailing separator before the closing delimiter is a syntax error for
    /// `path` but accepted for `lseg` — the two types genuinely differ here.
    #[test]
    fn trailing_separator_differs_between_path_and_lseg() -> anyhow::Result<()> {
        for lenient in ["[(1,2),(3,4),]", "((1,2),(3,4),)", "(1,2,3,4,)", "[1,2,3,4,]"] {
            assert_eq!(parse_lseg(lenient)?, [1.0, 2.0, 3.0, 4.0], "{lenient}");
            assert_eq!(
                parse_path(lenient).unwrap_err().sqlstate,
                "22P02",
                "path must reject {lenient}"
            );
        }
        // Both reject a separator outside the closing delimiter, and lseg still
        // rejects a list that runs out of points.
        assert_eq!(parse_lseg("[(1,2),(3,4)],").unwrap_err().sqlstate, "22P02");
        assert_eq!(parse_path("[(1,2),(3,4)],").unwrap_err().sqlstate, "22P02");
        assert_eq!(parse_lseg("[(1,2),]").unwrap_err().sqlstate, "22P02");

        Ok(())
    }

    /// A one-point *closed* path carries a degenerate zero-length segment; a
    /// one-point *open* path has no segments at all.
    #[test]
    fn one_point_paths() -> anyhow::Result<()> {
        let closed1 = parse_path("((10,20))")?;
        let open1 = parse_path("[(10,20)]")?;
        assert!(on_ppath(&[10.0, 20.0], &closed1));
        assert!(path_contain_pt(&closed1, &[10.0, 20.0]));
        // An open one-point path has no segment, so nothing lies on it.
        assert!(!on_ppath(&[10.0, 20.0], &open1));
        assert!(!on_ppath(&[0.0, 0.0], &closed1));
        // Its degenerate segment meets a real segment running through it, but
        // never another degenerate segment — even the identical one.
        assert!(path_inter(
            &parse_path("((0,0))")?,
            &parse_path("((0,0),(1,1))")?
        ));
        assert!(path_inter(
            &parse_path("((1,1))")?,
            &parse_path("((0,0),(2,2))")?
        ));
        assert!(!path_inter(
            &parse_path("((5,5))")?,
            &parse_path("((0,0),(1,1))")?
        ));
        assert!(!path_inter(&closed1, &closed1));
        assert!(!path_inter(&open1, &open1));
        // Length and area stay 0 (and +0, not -0).
        assert_eq!(path_length(&closed1), 0.0);
        assert!(path_length(&closed1).is_sign_positive());
        assert_eq!(path_area(&closed1), Some(0.0));

        Ok(())
    }

    #[test]
    fn path_bad_input() {
        for bad in [
            "[]",
            "[(,2),(3,4)]",
            "[(1,2),(3,4)",
            "(1,2,3,4",
            "(1,2),(3,4)]",
            "[(1,2),(3)]",
            "[(1,2,6),(3,4,6)]",
        ] {
            let e = parse_path(bad).unwrap_err();
            assert_eq!(e.sqlstate, "22P02", "{bad}");
            assert_eq!(
                e.message,
                format!("invalid input syntax for type path: \"{bad}\"")
            );
        }
    }

    #[test]
    fn path_predicates_and_conversions() -> anyhow::Result<()> {
        let open = parse_path("[(1,2),(3,4)]")?;
        let closed = parse_path("((1,2),(3,4))")?;
        assert!(path_isopen(&open) && !path_isclosed(&open));
        assert!(path_isclosed(&closed) && !path_isopen(&closed));
        assert_eq!(format_path(&path_pclose(&open), 0), "((1,2),(3,4))");
        assert_eq!(format_path(&path_popen(&closed), 0), "[(1,2),(3,4)]");
        // A single-point path converts both ways too.
        assert_eq!(format_path(&path_popen(&parse_path("((10,20))")?), 0), "[(10,20)]");
        assert_eq!(path_npoints(&parse_path("[(0,0),(3,0),(4,5),(1,6)]")?), 4);

        Ok(())
    }

    #[test]
    fn path_length_and_area() -> anyhow::Result<()> {
        // A closed path adds the segment from the last vertex back to the first.
        assert_eq!(path_length(&parse_path("[(0,0),(3,0),(3,4)]")?), 7.0);
        assert_eq!(path_length(&parse_path("((0,0),(3,0),(3,4))")?), 12.0);
        // A path with no segments is +0, not -0: the sign reaches the output.
        let len1 = path_length(&parse_path("[(1,2)]")?);
        assert_eq!(len1, 0.0);
        assert!(len1.is_sign_positive(), "one-point path length must be +0");
        assert!(path_length(&parse_path("((1,2))")?).is_sign_positive());

        assert_eq!(path_area(&parse_path("((0,0),(4,0),(4,3))")?), Some(6.0));
        // The shoelace sign depends on winding; the area does not.
        assert_eq!(
            path_area(&parse_path("((0,0),(0,3),(4,3),(4,0))")?),
            Some(12.0)
        );
        assert_eq!(path_area(&parse_path("((1,2))")?), Some(0.0));
        assert_eq!(path_area(&parse_path("[(0,0),(4,0),(4,3)]")?), None);

        Ok(())
    }

    #[test]
    fn path_arithmetic() -> anyhow::Result<()> {
        let p = parse_path("[(1,2),(3,4)]")?;
        assert_eq!(
            format_path(&path_add_pt(&p, &[1.0, 1.0])?, 0),
            "[(2,3),(4,5)]"
        );
        assert_eq!(
            format_path(&path_sub_pt(&p, &[1.0, 1.0])?, 0),
            "[(0,1),(2,3)]"
        );
        assert_eq!(
            format_path(&path_mul_pt(&p, &[2.0, 0.0])?, 0),
            "[(2,4),(6,8)]"
        );
        assert_eq!(
            format_path(&path_div_pt(&p, &[2.0, 0.0])?, 0),
            "[(0.5,1),(1.5,2)]"
        );
        assert_eq!(path_div_pt(&p, &[0.0, 0.0]).unwrap_err().sqlstate, "22012");

        Ok(())
    }

    #[test]
    fn path_concat_requires_open_operands() -> anyhow::Result<()> {
        let open = parse_path("[(0,0),(1,0)]")?;
        let closed = parse_path("((2,2),(3,3))")?;
        let other = parse_path("[(2,2),(3,3)]")?;
        assert_eq!(
            path_concat(&open, &other).map(|p| format_path(&p, 0)),
            Some("[(0,0),(1,0),(2,2),(3,3)]".to_string())
        );
        assert_eq!(path_concat(&open, &closed), None);
        assert_eq!(path_concat(&closed, &open), None);

        Ok(())
    }

    #[test]
    fn path_distance_and_intersection() -> anyhow::Result<()> {
        let a = parse_path("[(0,0),(1,1)]")?;
        let b = parse_path("[(3,0),(4,1)]")?;
        assert_eq!(path_distance(&a, &b), Some(5f64.sqrt()));
        assert_eq!(
            path_distance(
                &parse_path("((0,0),(1,0),(1,1))")?,
                &parse_path("((5,0),(6,0))")?
            ),
            Some(4.0)
        );
        // No segments at all on one side: no distance, like PG's NULL. A closed
        // one-point path DOES have a (degenerate) segment, so it measures.
        assert_eq!(
            path_distance(&parse_path("[(0,0)]")?, &parse_path("[(9,9)]")?),
            None
        );
        assert_eq!(
            path_distance(&parse_path("[(0,0)]")?, &parse_path("((9,9))")?),
            None
        );
        assert_eq!(
            path_distance(&parse_path("((0,0))")?, &parse_path("((3,4))")?),
            Some(5.0)
        );

        assert_eq!(dist_path_point(&a, &[2.0, 0.0]), 2f64.sqrt());
        // A one-point OPEN path has no segment to measure against: PG says 0.
        // A one-point CLOSED path measures against its degenerate segment.
        assert_eq!(dist_path_point(&parse_path("[(0,0)]")?, &[3.0, 4.0]), 0.0);
        assert_eq!(dist_path_point(&parse_path("((0,0))")?, &[3.0, 4.0]), 5.0);

        assert!(path_inter(
            &parse_path("[(0,0),(2,0)]")?,
            &parse_path("[(1,-1),(1,1)]")?
        ));
        assert!(!path_inter(
            &parse_path("[(0,0),(2,0)]")?,
            &parse_path("[(0,1),(2,1)]")?
        ));

        Ok(())
    }

    #[test]
    fn path_containment() -> anyhow::Result<()> {
        let closed_square = parse_path("((0,0),(4,0),(4,4),(0,4))")?;
        let open_square = parse_path("[(0,0),(4,0),(4,4),(0,4)]")?;
        assert!(path_contain_pt(&closed_square, &[1.0, 1.0]));
        // Boundary and vertex points count as contained.
        assert!(path_contain_pt(&closed_square, &[0.0, 2.0]));
        assert!(path_contain_pt(&closed_square, &[4.0, 4.0]));
        assert!(!path_contain_pt(&closed_square, &[5.0, 5.0]));
        // An open path does not enclose its interior...
        assert!(!path_contain_pt(&open_square, &[1.0, 1.0]));
        // ...but `@>` is the commutator of `<@`, so points ON an open path's
        // outline ARE contained.
        assert!(path_contain_pt(&open_square, &[2.0, 0.0]));
        assert!(path_contain_pt(&parse_path("[(0,0),(4,0)]")?, &[1.0, 0.0]));
        // The two spellings must agree for every pair.
        for p in [&closed_square, &open_square] {
            for q in [[1.0, 1.0], [2.0, 0.0], [5.0, 5.0], [0.0, 2.0]] {
                assert_eq!(path_contain_pt(p, &q), on_ppath(&q, p), "{p:?} vs {q:?}");
            }
        }

        // `<@` on an open path is an on-the-outline test...
        assert!(on_ppath(&[1.0, 0.0], &parse_path("[(0,0),(2,0)]")?));
        assert!(!on_ppath(&[1.0, 1.0], &open_square));
        // ...but on a closed path it is the same region test as `@>`.
        assert!(on_ppath(&[1.0, 3.0], &parse_path("((0,0),(2,0),(2,6))")?));
        assert!(on_ppath(&[1.0, 1.0], &closed_square));

        Ok(())
    }

    #[test]
    fn path_comparisons_use_the_point_count() -> anyhow::Result<()> {
        use std::cmp::Ordering;
        let two_open = parse_path("[(0,0),(1,1)]")?;
        let two_closed = parse_path("((5,5),(6,6))")?;
        let three = parse_path("[(0,0),(1,1),(2,2)]")?;
        assert_eq!(path_n_cmp(&two_open, &two_closed), Ordering::Equal);
        assert_eq!(path_n_cmp(&three, &two_closed), Ordering::Greater);
        assert_eq!(path_n_cmp(&two_open, &three), Ordering::Less);

        Ok(())
    }

    // -- box ---------------------------------------------------------------

    #[test]
    fn box_forms_and_normalization() -> anyhow::Result<()> {
        // All four spellings parse, and the corners come out in normal form
        // (high componentwise >= low), which is what PG prints.
        for s in ["(1,2,3,4)", "((1,2),(3,4))", "(1,2),(3,4)", "1,2,3,4"] {
            assert_eq!(parse_box(s)?, [3.0, 4.0, 1.0, 2.0], "{s}");
        }
        // The swap is per coordinate, so a "mixed" corner pair normalizes too.
        assert_eq!(parse_box("(0,2,2,0)")?, [2.0, 2.0, 0.0, 0.0]);
        assert_eq!(parse_box(" ( ( 1 , 2 ) , ( 3 , 4 ) ) ")?, [3.0, 4.0, 1.0, 2.0]);
        // Unlike `lseg`, a trailing separator is tolerated but `[...]` is not.
        assert_eq!(parse_box("(1,2),(3,4),")?, [3.0, 4.0, 1.0, 2.0]);
        assert_eq!(format_box(&[3.0, 4.0, 1.0, 2.0], 0), "(3,4),(1,2)");
        Ok(())
    }

    #[test]
    fn box_bad_input() {
        for s in ["[1, 2, 3, 4)", "(1, 2, 3, 4]", "(2.3, 4.5)", "(1, 2, 3, 4) x", "asdfasdf(ad"] {
            let e = parse_box(s).expect_err(s);
            assert_eq!(e.sqlstate, "22P02", "{s}");
            assert_eq!(e.message, format!("invalid input syntax for type box: \"{s}\""));
        }
    }

    #[test]
    fn box_measures_and_conversions() -> anyhow::Result<()> {
        let b = parse_box("(0,0,2,3)")?;
        assert_eq!(box_area(&b), 6.0);
        assert_eq!(box_width(&b), 2.0);
        assert_eq!(box_height(&b), 3.0);
        assert_eq!(box_center(&b), [1.0, 1.5]);
        // `diagonal` / `::lseg` run high corner to low corner.
        assert_eq!(box_diagonal(&b), [2.0, 3.0, 0.0, 0.0]);
        assert_eq!(bound_box(&parse_box("(0,0,1,1)")?, &parse_box("(5,5,6,6)")?), [6.0, 6.0, 0.0, 0.0]);
        // The circumscribed circle, and back to the inscribed box.
        let c = box_to_circle(&parse_box("(0,0,2,2)")?);
        assert_eq!(c[0..2], [1.0, 1.0]);
        assert!((c[2] - 2.0_f64.sqrt()).abs() < 1e-12);
        assert_eq!(circle_to_box(&[0.0, 0.0, 2.0])[0], 2.0 / 2.0_f64.sqrt());
        assert_eq!(
            box_to_polygon(&parse_box("(0,0,2,3)")?).pts,
            vec![[0.0, 0.0], [0.0, 3.0], [2.0, 3.0], [2.0, 0.0]]
        );
        Ok(())
    }

    #[test]
    fn box_predicates_and_distances() -> anyhow::Result<()> {
        let a = parse_box("(0,0,2,2)")?;
        let b = parse_box("(1,1,3,3)")?;
        assert!(box_overlap(&a, &b));
        assert!(box_over_left(&a, &b) && !box_over_right(&a, &b));
        assert!(box_over_below(&a, &b) && !box_over_above(&a, &b));
        assert!(!box_left(&a, &b) && !box_right(&a, &b));
        assert!(!box_same(&a, &b) && box_same(&a, &a));
        assert_eq!(box_intersect(&a, &b), Some([2.0, 2.0, 1.0, 1.0]));
        assert_eq!(box_intersect(&a, &parse_box("(5,5,6,6)")?), None);
        // `=` is by *area*, so two differently placed boxes can compare equal.
        assert_eq!(box_area_cmp(&a, &b), std::cmp::Ordering::Equal);
        assert_eq!(box_area_cmp(&a, &parse_box("(0,0,3,3)")?), std::cmp::Ordering::Less);
        assert!(box_below_eq(&a, &parse_box("(0,5,2,7)")?));
        assert!(box_contain_pt(&a, &[1.0, 1.0]) && !box_contain_pt(&a, &[5.0, 5.0]));
        // `box <-> box` is measured **center to center**, not outline to outline.
        assert_eq!(dist_box_box(&a, &parse_box("(10,0,12,2)")?), 10.0);
        assert_eq!(dist_point_box(&[1.0, 1.0], &a), 0.0);
        assert_eq!(close_point_box(&[1.0, 1.0], &a), [1.0, 1.0]);
        assert_eq!(close_point_box(&[5.0, 5.0], &a), [2.0, 2.0]);
        Ok(())
    }

    #[test]
    fn lseg_box_interaction() -> anyhow::Result<()> {
        let b = parse_box("(0,0,10,4)")?;
        assert!(lseg_inside_box(&parse_lseg("[(1,1),(2,2)]")?, &b));
        assert!(!lseg_inside_box(&parse_lseg("[(1,1),(20,2)]")?, &b));
        assert!(lseg_intersects_box(&parse_lseg("[(-5,2),(1,2)]")?, &b));
        assert!(!lseg_intersects_box(&parse_lseg("[(-5,2),(-1,2)]")?, &b));
        // When the segment meets the box, `##` reports the point of the
        // *segment* nearest the box center — including when it is clamped to an
        // endpoint.
        assert_eq!(close_lseg_box(&parse_lseg("[(1,1),(2,2)]")?, &b), [2.0, 2.0]);
        assert_eq!(close_lseg_box(&parse_lseg("[(9,3),(9.5,3.5)]")?, &b), [9.0, 3.0]);
        assert_eq!(close_lseg_box(&parse_lseg("[(-5,2),(15,2)]")?, &b), [5.0, 2.0]);
        // Otherwise it is the point of the box outline nearest the segment.
        let small = parse_box("(0,0,2,2)")?;
        assert_eq!(close_lseg_box(&parse_lseg("[(3,1),(4,1)]")?, &small), [2.0, 1.0]);
        assert_eq!(close_lseg_box(&parse_lseg("[(5,5),(6,6)]")?, &small), [2.0, 2.0]);
        assert_eq!(dist_lseg_box(&parse_lseg("[(1,1),(2,2)]")?, &b), 0.0);
        Ok(())
    }

    // -- line --------------------------------------------------------------

    #[test]
    fn line_forms_and_format() -> anyhow::Result<()> {
        assert_eq!(parse_line("{0,-1,5}")?, [0.0, -1.0, 5.0]);
        assert!(parse_line("{3,NaN,5}")?[1].is_nan());
        // The two-point spellings all normalize through `line_from_points`.
        assert_eq!(parse_line(" (0,0), (6,6)")?, [1.0, -1.0, 0.0]);
        assert_eq!(parse_line("10,-10 ,-5,-4")?, [-0.4, -1.0, -6.0]);
        // Horizontal and vertical get PG's canonical `{0,-1,y}` / `{-1,0,x}`.
        assert_eq!(parse_line("[(1,3),(2,3)]")?, [0.0, -1.0, 3.0]);
        assert_eq!(line_from_points(&[3.0, 1.0], &[3.0, 2.0])?, [-1.0, 0.0, 3.0]);
        assert_eq!(format_line(&[0.0, -1.0, 5.0], 0), "{0,-1,5}");
        Ok(())
    }

    #[test]
    fn line_bad_input() {
        for s in ["{}", "{0", "{0,0}", "{0,0,1", "{0,0,1} x", "(3asdf,2 ,3,4r2)", "[1,2,3, 4"] {
            let e = parse_line(s).expect_err(s);
            assert_eq!(e.sqlstate, "22P02", "{s}");
            assert_eq!(e.message, format!("invalid input syntax for type line: \"{s}\""), "{s}");
        }
        // The two spec errors carry their own wording, not the generic one.
        assert_eq!(
            parse_line("{0,0,1}").expect_err("A and B zero").message,
            "invalid line specification: A and B cannot both be zero"
        );
        assert_eq!(
            parse_line("[(1,2),(1,2)]").expect_err("coincident").message,
            "invalid line specification: must be two distinct points"
        );
    }

    #[test]
    fn line_equality_is_scale_invariant_except_for_nan() -> anyhow::Result<()> {
        assert!(line_eq(&parse_line("{1,2,3}")?, &parse_line("{2,4,6}")?));
        assert!(line_eq(&parse_line("{1,2,3}")?, &parse_line("{-1,-2,-3}")?));
        assert!(!line_eq(&parse_line("{1,2,3}")?, &parse_line("{1,2,4}")?));
        // With a NaN anywhere, PG drops the ratio test and insists on exact
        // equality (with `NaN = NaN`), so scaling no longer preserves equality.
        assert!(line_eq(&parse_line("{nan,1,nan}")?, &parse_line("{nan,1,nan}")?));
        assert!(!line_eq(&parse_line("{nan,1,nan}")?, &parse_line("{nan,2,nan}")?));
        assert!(!line_eq(&parse_line("{3,NaN,5}")?, &parse_line("{6,NaN,10}")?));
        Ok(())
    }

    #[test]
    fn line_predicates_and_geometry() -> anyhow::Result<()> {
        let diag = parse_line("{1,-1,0}")?; // y = x
        let anti = parse_line("{1,1,-2}")?; // y = 2 - x
        assert!(line_horizontal(&parse_line("{0,-1,5}")?));
        assert!(line_vertical(&parse_line("{1,0,-5}")?));
        assert!(line_parallel(&diag, &parse_line("{1,-1,5}")?));
        assert!(line_perpendicular(&diag, &anti));
        assert_eq!(line_interpt(&diag, &anti), Some([1.0, 1.0]));
        // Parallel lines — including two spellings of the same line — have no
        // single intersection point.
        assert_eq!(line_interpt(&diag, &parse_line("{2,-2,0}")?), None);
        assert!(!line_intersects(&diag, &parse_line("{1,-1,5}")?));
        assert!(point_on_line(&[1.0, 1.0], &diag) && !point_on_line(&[1.0, 2.0], &diag));
        assert_eq!(close_point_line(&[0.0, 5.0], &diag), [2.5, 2.5]);
        assert!((dist_point_line(&[0.0, 5.0], &diag) - 5.0 / 2.0_f64.sqrt()).abs() < 1e-12);
        // Distance is 0 unless the lines are parallel.
        assert_eq!(dist_line_line(&diag, &anti), 0.0);
        assert!((dist_line_line(&diag, &parse_line("{1,-1,5}")?) - 5.0 / 2.0_f64.sqrt()).abs() < 1e-12);
        assert!(line_intersects_box(&diag, &parse_box("(0,0,2,2)")?));
        assert!(!line_intersects_box(&parse_line("{1,-1,-10}")?, &parse_box("(0,0,2,2)")?));
        Ok(())
    }

    #[test]
    fn line_lseg_interaction() -> anyhow::Result<()> {
        let diag = parse_line("{1,-1,0}")?;
        assert!(lseg_on_line(&parse_lseg("[(0,0),(1,1)]")?, &diag));
        assert!(lseg_intersects_line(&parse_lseg("[(0,0),(1,1)]")?, &parse_line("{1,1,-2}")?));
        assert!(!lseg_intersects_line(&parse_lseg("[(0,0),(1,1)]")?, &parse_line("{1,-1,5}")?));
        // `line ## lseg` picks the point of the segment closest to the line —
        // the crossing when there is one, else the nearer endpoint.
        assert_eq!(close_line_lseg(&diag, &parse_lseg("[(0,5),(5,5)]")?), Some([5.0, 5.0]));
        assert_eq!(close_line_lseg(&diag, &parse_lseg("[(3,0),(4,0)]")?), Some([3.0, 0.0]));
        // A segment running parallel to the line (including lying on it) has no
        // single closest point.
        assert_eq!(close_line_lseg(&diag, &parse_lseg("[(0,0),(1,1)]")?), None);
        assert_eq!(close_line_lseg(&diag, &parse_lseg("[(0,5),(1,6)]")?), None);
        Ok(())
    }

    // -- circle ------------------------------------------------------------

    #[test]
    fn circle_forms_and_format() -> anyhow::Result<()> {
        for s in ["<(1,2),3>", "((1,2),3)", "(1,2),3", "1,2,3", " < ( 1 , 2 ) , 3 > "] {
            assert_eq!(parse_circle(s)?, [1.0, 2.0, 3.0], "{s}");
        }
        // Zero and NaN radii are accepted; a negative one is not.
        assert_eq!(parse_circle("<(3,5),0>")?, [3.0, 5.0, 0.0]);
        assert!(parse_circle("<(3,5),NaN>")?[2].is_nan());
        assert_eq!(format_circle(&[5.0, 1.0, 3.0], 0), "<(5,1),3>");
        Ok(())
    }

    #[test]
    fn circle_bad_input() {
        for s in ["<(-100,0),-100>", "<(100,200),10", "<(100,200),10> x", "1abc,3,5", "(3,(1,2),3)"] {
            let e = parse_circle(s).expect_err(s);
            assert_eq!(e.sqlstate, "22P02", "{s}");
            assert_eq!(e.message, format!("invalid input syntax for type circle: \"{s}\""), "{s}");
        }
    }

    #[test]
    fn circle_measures_and_conversions() -> anyhow::Result<()> {
        let c = parse_circle("<(5,1),3>")?;
        assert_eq!(circle_center(&c), [5.0, 1.0]);
        assert_eq!(circle_radius(&c), 3.0);
        assert_eq!(circle_diameter(&c), 6.0);
        assert!((circle_area(&c) - std::f64::consts::PI * 9.0).abs() < 1e-12);
        // `polygon(n, circle)` starts at the leftmost point and runs clockwise.
        let quad = circle_to_polygon(4, &[0.0, 0.0, 1.0])?;
        assert_eq!(quad.pts.len(), 4);
        assert_eq!(quad.pts[0], [-1.0, 0.0]);
        assert!((quad.pts[1][1] - 1.0).abs() < 1e-12);
        assert_eq!(circle_to_polygon(1, &[0.0, 0.0, 1.0]).expect_err("too few").sqlstate, "22023");
        // `circle(polygon)`: vertex centroid, mean vertex distance.
        let sq = parse_polygon("((0,0),(2,0),(2,2),(0,2))")?;
        let cc = circle_from_polygon(&sq);
        assert_eq!(cc[0..2], [1.0, 1.0]);
        assert!((cc[2] - 2.0_f64.sqrt()).abs() < 1e-12);
        Ok(())
    }

    #[test]
    fn circle_predicates_and_distances() -> anyhow::Result<()> {
        let a = parse_circle("<(0,0),2>")?;
        let b = parse_circle("<(1,1),1>")?;
        assert!(circle_overlap(&a, &b));
        assert!(circle_over_left(&a, &b) && !circle_over_right(&a, &b));
        assert!(circle_over_below(&a, &b) && !circle_over_above(&a, &b));
        assert!(!circle_contain(&a, &b) && !circle_contained(&a, &b));
        assert!(circle_contain(&a, &parse_circle("<(0,0),1>")?));
        assert!(!circle_same(&a, &b) && circle_same(&a, &a));
        assert!(circle_contain_pt(&a, &[1.0, 1.0]) && !circle_contain_pt(&a, &[5.0, 5.0]));
        // `=` compares area, so any two same-radius circles are equal.
        assert_eq!(circle_area_cmp(&a, &parse_circle("<(9,9),2>")?), std::cmp::Ordering::Equal);
        assert_eq!(circle_area_cmp(&a, &b), std::cmp::Ordering::Greater);
        // Distances measure outline to outline and clamp at 0.
        assert_eq!(dist_circle_circle(&a, &b), 0.0);
        assert!((dist_circle_circle(&a, &parse_circle("<(5,5),1>")?) - (50.0_f64.sqrt() - 3.0)).abs() < 1e-12);
        assert_eq!(dist_point_circle(&[1.0, 1.0], &a), 0.0);
        // Arithmetic moves the center; `*` and `/` also scale the radius.
        assert_eq!(circle_add_pt(&a, &[1.0, 1.0])?, [1.0, 1.0, 2.0]);
        assert_eq!(circle_mul_pt(&a, &[2.0, 0.0])?, [0.0, 0.0, 4.0]);
        assert_eq!(circle_div_pt(&a, &[2.0, 0.0])?, [0.0, 0.0, 1.0]);
        Ok(())
    }

    // -- polygon -----------------------------------------------------------

    #[test]
    fn polygon_forms_and_format() -> anyhow::Result<()> {
        for s in ["((0,0),(2,0),(2,2))", "(0,0),(2,0),(2,2)", "0,0,2,0,2,2"] {
            assert_eq!(parse_polygon(s)?.pts, vec![[0.0, 0.0], [2.0, 0.0], [2.0, 2.0]], "{s}");
        }
        assert_eq!(parse_polygon("(0,0)")?.pts, vec![[0.0, 0.0]]);
        assert_eq!(
            format_polygon(&parse_polygon("((0,0),(2,0),(2,2))")?, 0),
            "((0,0),(2,0),(2,2))"
        );
        // The open-path spelling is not a polygon, and an unbalanced list fails.
        for s in ["[(0,0),(1,1)]", "((0,0)"] {
            let e = parse_polygon(s).expect_err(s);
            assert_eq!(e.sqlstate, "22P02", "{s}");
            assert_eq!(e.message, format!("invalid input syntax for type polygon: \"{s}\""), "{s}");
        }
        Ok(())
    }

    #[test]
    fn polygon_measures_and_conversions() -> anyhow::Result<()> {
        let sq = parse_polygon("((0,0),(2,0),(2,2),(0,2))")?;
        assert_eq!(poly_npoints(&sq), 4);
        assert_eq!(poly_center(&sq), [1.0, 1.0]);
        assert_eq!(poly_bbox(&sq), [2.0, 2.0, 0.0, 0.0]);
        assert_eq!(poly_center(&parse_polygon("((0,0),(4,0),(4,4))")?)[0], 8.0 / 3.0);
        // `polygon::path` is always *closed*; only a closed path converts back.
        assert!(poly_to_path(&sq).closed);
        assert_eq!(path_to_polygon(&parse_path("((0,0),(1,1),(2,0))")?)?.pts.len(), 3);
        let e = path_to_polygon(&parse_path("[(0,0),(1,1),(2,0)]")?).expect_err("open");
        assert_eq!(e.sqlstate, "22023");
        assert_eq!(e.message, "open path cannot be converted to polygon");
        Ok(())
    }

    #[test]
    fn polygon_predicates_and_distances() -> anyhow::Result<()> {
        let outer = parse_polygon("((0,0),(4,0),(4,4),(0,4))")?;
        let inner = parse_polygon("((1,1),(2,1),(2,2),(1,2))")?;
        assert!(poly_contain(&outer, &inner) && poly_contained(&inner, &outer));
        assert!(poly_contain(&outer, &outer));
        // Sharing the bounding box but poking outside it is not containment.
        assert!(!poly_contain(&outer, &parse_polygon("((-1,-1),(5,0),(4,4))")?));
        assert!(poly_contain(&outer, &parse_polygon("((0,0),(4,0),(4,4))")?));
        assert!(poly_same(&outer, &outer) && !poly_same(&outer, &inner));
        assert!(poly_overlap(&outer, &inner));
        assert!(!poly_overlap(
            &parse_polygon("((0,0),(1,0),(0,1))")?,
            &parse_polygon("((3,3),(2,3),(3,2))")?
        ));
        assert!(poly_contain_pt(&outer, &[1.0, 1.0]) && !poly_contain_pt(&outer, &[5.0, 5.0]));
        assert!(poly_left(&parse_polygon("((0,0),(1,0),(1,1))")?, &parse_polygon("((5,0),(6,0),(6,1))")?));
        assert_eq!(dist_poly_point(&outer, &[2.0, 2.0]), 0.0);
        let far = parse_polygon("((5,5),(6,5),(6,6))")?;
        let two_sq = parse_polygon("((0,0),(2,0),(2,2),(0,2))")?;
        assert!((dist_poly_point(&two_sq, &[5.0, 5.0]) - 18.0_f64.sqrt()).abs() < 1e-12);
        assert!((dist_poly_poly(&two_sq, &far) - 18.0_f64.sqrt()).abs() < 1e-12);
        assert!((dist_poly_circle(&two_sq, &parse_circle("<(5,5),1>")?) - (18.0_f64.sqrt() - 1.0)).abs() < 1e-12);
        assert_eq!(dist_poly_poly(&outer, &inner), 0.0);
        Ok(())
    }
}
