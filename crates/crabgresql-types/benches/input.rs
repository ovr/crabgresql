//! Input-function microbenchmarks: the text → `Value` conversions a bulk load
//! runs once per cell.
//!
//! A COPY of a wide table spends a measurable share of its time here — a load
//! profile put `text_to_int` + `intlit::scan` at ~5.6%, `timestamp::parse_parts`
//! at 1.3% and the char-based `trim_matches` inside them at 1.1%. The cases
//! below are the shapes a load actually carries (a bare decimal, an ISO date, an
//! ISO timestamp), with the awkward spellings kept alongside so an optimization
//! that only helps the common form does not silently cost the rest.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use crabgresql_types::cast::text_to_int;
use crabgresql_types::fmt::FmtCtx;
use crabgresql_types::{Numeric, PgType, date, float, intlit, timestamp};

fn int_in(c: &mut Criterion) {
    let mut g = c.benchmark_group("int_in");

    // The overwhelmingly common shapes: a short bare decimal, in the two widths
    // a load actually declares.
    g.bench_function("int4_small", |b| {
        b.iter(|| text_to_int(black_box("12345"), PgType::Int4))
    });
    g.bench_function("int4_zero", |b| {
        b.iter(|| text_to_int(black_box("0"), PgType::Int4))
    });
    // The most negative value of the width: the sign path plus the magnitude
    // that only survives because the range check is unsigned.
    g.bench_function("int4_min", |b| {
        b.iter(|| text_to_int(black_box("-2147483648"), PgType::Int4))
    });
    // 19 digits — the widest decimal an `int8` holds, and the boundary any
    // register-sized fast path has to stop at.
    g.bench_function("int8_max", |b| {
        b.iter(|| text_to_int(black_box("9223372036854775807"), PgType::Int8))
    });
    // Surrounding whitespace, which is what the trim costs.
    g.bench_function("int4_spaced", |b| {
        b.iter(|| text_to_int(black_box("  42  "), PgType::Int4))
    });
    // The spellings that must keep working: separators and a radix prefix.
    g.bench_function("int4_underscores", |b| {
        b.iter(|| text_to_int(black_box("1_000_000"), PgType::Int4))
    });
    g.bench_function("int4_hex", |b| {
        b.iter(|| text_to_int(black_box("0x42F"), PgType::Int4))
    });
    // The rejection path, which formats a message: a load with one bad column
    // pays this per row until it aborts.
    g.bench_function("int4_syntax_error", |b| {
        b.iter(|| text_to_int(black_box("12abc"), PgType::Int4))
    });
    g.bench_function("int2_out_of_range", |b| {
        b.iter(|| text_to_int(black_box("70000"), PgType::Int2))
    });

    g.finish();
}

/// The scanner alone, so its cost separates from `text_to_int`'s range check
/// and error formatting.
fn intlit_scan(c: &mut Criterion) {
    let mut g = c.benchmark_group("intlit_scan");

    for (name, text) in [
        ("small", "12345"),
        ("negative", "-2147483648"),
        ("wide", "9223372036854775807"),
        ("spaced", "  42  "),
        ("underscores", "1_000_000"),
        ("hex", "0x42F"),
    ] {
        g.bench_function(name, |b| {
            b.iter(|| intlit::scan_int_literal(black_box(text)))
        });
    }

    g.finish();
}

fn float_in(c: &mut Criterion) {
    let mut g = c.benchmark_group("float_in");

    g.bench_function("float8_decimal", |b| {
        b.iter(|| float::float8in(black_box("3.14159")))
    });
    g.bench_function("float8_exponent", |b| {
        b.iter(|| float::float8in(black_box("-1e300")))
    });
    g.bench_function("float8_spaced", |b| {
        b.iter(|| float::float8in(black_box(" 42 ")))
    });
    g.bench_function("float4_decimal", |b| {
        b.iter(|| float::float4in(black_box("1.5")))
    });

    g.finish();
}

fn numeric_in(c: &mut Criterion) {
    let mut g = c.benchmark_group("numeric_in");

    g.bench_function("scaled", |b| b.iter(|| Numeric::parse(black_box("123.45"))));
    g.bench_function("zero", |b| b.iter(|| Numeric::parse(black_box("0"))));
    g.bench_function("wide", |b| {
        b.iter(|| Numeric::parse(black_box("123456789012345678901234567890.12345")))
    });

    g.finish();
}

fn date_in(c: &mut Criterion) {
    let mut g = c.benchmark_group("date_in");
    let fmt = FmtCtx::utc(0);

    // The ISO form every load carries.
    g.bench_function("iso", |b| {
        b.iter(|| date::parse(black_box("2013-07-15"), &fmt))
    });
    // The verbose form, whose comma is the one input that genuinely needs a
    // rewritten copy of the string.
    g.bench_function("verbose_comma", |b| {
        b.iter(|| date::parse(black_box("Jan 8, 1999"), &fmt))
    });

    g.finish();
}

fn timestamp_in(c: &mut Criterion) {
    let mut g = c.benchmark_group("timestamp_in");
    let fmt = FmtCtx::utc(0);

    g.bench_function("iso_space", |b| {
        b.iter(|| timestamp::parse(black_box("2013-07-15 10:20:30"), &fmt))
    });
    g.bench_function("iso_t_fraction", |b| {
        b.iter(|| timestamp::parse(black_box("2013-07-15T10:20:30.123456"), &fmt))
    });
    // A date with no time: the same input function, one field short.
    g.bench_function("date_only", |b| {
        b.iter(|| timestamp::parse(black_box("2013-07-15"), &fmt))
    });
    // The verbose form, which stays on the general scanner.
    g.bench_function("verbose", |b| {
        b.iter(|| timestamp::parse(black_box("Jan 8 1999 10:00:00"), &fmt))
    });

    g.finish();
}

criterion_group!(
    benches,
    int_in,
    intlit_scan,
    float_in,
    numeric_in,
    date_in,
    timestamp_in
);
criterion_main!(benches);
