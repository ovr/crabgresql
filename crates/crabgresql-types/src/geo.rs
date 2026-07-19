//! Geometric types: `point` and `lseg` (line segment).
//!
//! Clean-room reproduction of PostgreSQL's observable behavior (I/O text, error
//! text, SQLSTATE) for the geometric family. This module holds pure parse /
//! format / operator helpers; the runtime `Value` representations live in
//! [`crate`] (`Value::Point([f64; 2])`, `Value::Lseg([f64; 4])`).
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

fn fp_eq(a: f64, b: f64) -> bool {
    (a - b).abs() <= EPSILON
}
fn fp_lt(a: f64, b: f64) -> bool {
    b - a > EPSILON
}
fn fp_le(a: f64, b: f64) -> bool {
    a - b <= EPSILON
}
fn fp_gt(a: f64, b: f64) -> bool {
    a - b > EPSILON
}
fn fp_ge(a: f64, b: f64) -> bool {
    b - a <= EPSILON
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
        GeoError { sqlstate: e.sqlstate, message: e.message.to_string() }
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
    let starts_with_ci = |w: &[u8], kw: &[u8]| w.len() >= kw.len() && w[..kw.len()].eq_ignore_ascii_case(kw);
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
            GeoError { sqlstate: e.sqlstate, message: e.message }
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
fn path_decode(
    orig: &str,
    opentype: bool,
    npts: usize,
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
        // grouping paren around the whole list.
        let mut j = cur.i + 1;
        let b = orig.as_bytes();
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == b'(' {
            depth += 1;
            cur.i += 1;
        }
    }

    let mut pts = Vec::with_capacity(npts);
    for _ in 0..npts {
        let p = pair_decode(&mut cur, type_name, orig)?;
        pts.push(p);
        cur.skip_ws();
        cur.eat(b',');
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
    let (_open, pts) = path_decode(orig, true, 2, "lseg")?;
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

fn hypot(dx: f64, dy: f64) -> f64 {
    dx.hypot(dy)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_roundtrip_and_forms() {
        assert_eq!(parse_point("(1,2)").unwrap(), [1.0, 2.0]);
        assert_eq!(parse_point("1,2").unwrap(), [1.0, 2.0]);
        assert_eq!(parse_point(" ( -3.0 , 4.0 ) ").unwrap(), [-3.0, 4.0]);
        assert_eq!(format_point(&[5.1, 34.5], 0), "(5.1,34.5)");
        assert_eq!(format_point(&[0.0, 0.0], 0), "(0,0)");
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
    fn lseg_forms_and_format() {
        assert_eq!(parse_lseg("[(1,2),(3,4)]").unwrap(), [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(parse_lseg("(0,0),(6,6)").unwrap(), [0.0, 0.0, 6.0, 6.0]);
        assert_eq!(parse_lseg("10,-10 ,-3,-4").unwrap(), [10.0, -10.0, -3.0, -4.0]);
        assert_eq!(
            parse_lseg("[-1e6,2e2,3e5, -4e1]").unwrap(),
            [-1_000_000.0, 200.0, 300_000.0, -40.0]
        );
        assert_eq!(parse_lseg("((0,0),(1,0))").unwrap(), [0.0, 0.0, 1.0, 0.0]);
        assert_eq!(format_lseg(&[1.0, 2.0, 3.0, 4.0], 0), "[(1,2),(3,4)]");
    }

    #[test]
    fn lseg_bad_input() {
        for bad in ["(3asdf,2 ,3,4r2)", "[1,2,3, 4", "[(,2),(3,4)]", "[(1,2),(3,4)", "(1,2)"] {
            assert_eq!(parse_lseg(bad).unwrap_err().sqlstate, "22P02", "{bad}");
        }
    }

    #[test]
    fn point_ops() {
        assert_eq!(point_distance(&[0.0, 0.0], &[3.0, 4.0]), 5.0);
        assert!(point_left(&[-10.0, 0.0], &[0.0, 0.0]));
        assert!(point_eq(&[5.1, 34.5], &[5.1, 34.5]));
        assert!(point_eq(&[0.0, 0.0], &[0.000_000_9, 0.000_000_9]));
        assert!(!point_eq(&[0.0, 0.0], &[0.000_001_8, 0.000_001_8]));
        assert_eq!(point_mul(&[5.1, 34.5], &[-10.0, 0.0]).unwrap(), [-51.0, -345.0]);
        // Underflow: 1e-300 * 1e-300 underflows to 0 from nonzero inputs.
        assert_eq!(
            point_mul(&[1e-300, -1e-300], &[1e-300, -1e-300]).unwrap_err().sqlstate,
            "22003"
        );
        assert_eq!(point_div(&[5.1, 34.5], &[5.1, 34.5]).unwrap(), [1.0, 0.0]);
        assert_eq!(point_div(&[1.0, 1.0], &[0.0, 0.0]).unwrap_err().sqlstate, "22012");
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
        assert_eq!(dist_seg_seg(&[0.0, 0.0, 1.0, 0.0], &[0.0, 2.0, 1.0, 2.0]), 2.0);
        assert_eq!(
            lseg_interpt(&[0.0, 0.0, 2.0, 0.0], &[1.0, -1.0, 1.0, 1.0]),
            Some([1.0, 0.0])
        );
        assert_eq!(lseg_interpt(&[0.0, 0.0, 2.0, 0.0], &[0.0, 1.0, 2.0, 1.0]), None);
        assert_eq!(close_point_seg(&[0.0, 5.0], &[0.0, 0.0, 10.0, 0.0]), [0.0, 0.0]);
        // `##` is NULL (None) for parallel segments, a point otherwise.
        assert_eq!(close_seg_seg(&[0.0, 0.0, 1.0, 0.0], &[0.0, 2.0, 1.0, 2.0]), None);
        assert_eq!(
            close_seg_seg(&[0.0, 0.0, 2.0, 0.0], &[1.0, 1.0, 1.0, 3.0]),
            Some([1.0, 1.0])
        );
    }

    #[test]
    fn lseg_parallel_perpendicular_are_scale_invariant() {
        // Slope-based comparison stays correct at large coordinate magnitudes,
        // where an absolute cross/dot product would drift past EPSILON.
        assert!(lseg_parallel(&[0.0, 0.0, 1e6, 1.0], &[0.0, 0.0, 1e6, 1.000_000_1]));
        assert!(lseg_perpendicular(&[0.0, 0.0, 1000.0, 1.0], &[0.0, 0.0, 1.0, -999.999_5]));
        // Two vertical segments are parallel; vertical ⟂ horizontal.
        assert!(lseg_parallel(&[0.0, 0.0, 0.0, 5.0], &[3.0, 0.0, 3.0, 10.0]));
        assert!(lseg_perpendicular(&[0.0, 0.0, 0.0, 5.0], &[0.0, 0.0, 5.0, 0.0]));
    }
}
