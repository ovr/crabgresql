//! Geometric types: `point`, `lseg` (line segment) and `path` (point list).
//!
//! Clean-room reproduction of PostgreSQL's observable behavior (I/O text, error
//! text, SQLSTATE) for the geometric family. This module holds pure parse /
//! format / operator helpers; the runtime `Value` representations live in
//! [`crate`] (`Value::Point([f64; 2])`, `Value::Lseg([f64; 4])`,
//! `Value::Path(`[`PathVal`]`)`).
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
    /// The segments of the path, in order: consecutive vertex pairs plus, for a
    /// closed path, the segment from the last vertex back to the first. A
    /// one-point open path has no segments.
    fn segments(&self) -> impl Iterator<Item = [f64; 4]> + '_ {
        let n = self.pts.len();
        let closing = if self.closed && n > 1 { 1 } else { 0 };
        (0..n.saturating_sub(1) + closing).map(move |i| {
            let a = self.pts[i];
            let b = self.pts[(i + 1) % n];
            [a[0], a[1], b[0], b[1]]
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
    let (is_open, pts) = path_decode(orig, true, npts, "path")?;
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
/// to any segment. A one-point open path degenerates to the point distance.
pub fn dist_path_point(p: &PathVal, q: &[f64; 2]) -> f64 {
    let mut best = f64::INFINITY;
    let mut any = false;
    for s in p.segments() {
        best = best.min(dist_point_seg(q, &s));
        any = true;
    }
    if any {
        best
    } else {
        point_distance(q, &p.pts[0])
    }
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
/// for a closed path, PG treats the vertex list as a region, so this is the
/// same inside-or-on test as `@>`.
pub fn on_ppath(q: &[f64; 2], p: &PathVal) -> bool {
    if !p.closed {
        return point_on_path_boundary(q, p);
    }
    point_on_path_boundary(q, p) || point_inside(q, &p.pts)
}

/// `path @> point`: only a closed path contains points; boundary points count.
pub fn path_contain_pt(p: &PathVal, q: &[f64; 2]) -> bool {
    p.closed && (point_on_path_boundary(q, p) || point_inside(q, &p.pts))
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
        // No segments at all on either side: no distance, like PG's NULL.
        assert_eq!(
            path_distance(&parse_path("[(0,0)]")?, &parse_path("[(9,9)]")?),
            None
        );

        assert_eq!(dist_path_point(&a, &[2.0, 0.0]), 2f64.sqrt());
        assert_eq!(dist_path_point(&parse_path("[(0,0)]")?, &[3.0, 4.0]), 5.0);

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
        // An open path contains nothing, even when its outline would enclose.
        assert!(!path_contain_pt(&open_square, &[1.0, 1.0]));

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
}
