//! Input-function microbenchmarks: the text → `Value` conversions a bulk load
//! runs once per cell.
//!
//! A COPY of a wide table spends a measurable share of its time here, so the
//! cases below are the shapes a load actually carries — a bare decimal, an ISO
//! date, an ISO timestamp. The awkward spellings sit alongside them so an
//! optimization that only helps the common form does not silently cost the rest.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use crabgresql_types::cast::text_to_int;
use crabgresql_types::fmt::FmtCtx;
use crabgresql_types::{
    Numeric, PgType, date, float, intlit, macaddr, tid, timestamp, timestamptz, xid,
};

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
    // What a dump of a `timestamptz` column writes, and the shape a load of one
    // carries in every row.
    g.bench_function("iso_zone", |b| {
        b.iter(|| timestamp::parse(black_box("2013-07-15 10:20:30+00"), &fmt))
    });
    // The verbose forms, which stay on the general scanner.
    g.bench_function("verbose", |b| {
        b.iter(|| timestamp::parse(black_box("Jan 8 1999 10:00:00"), &fmt))
    });
    g.bench_function("verbose_zone", |b| {
        b.iter(|| timestamp::parse(black_box("Jan 8 1999 10:00:00 PST"), &fmt))
    });

    g.finish();
}

/// The same scan as `timestamp_in`, plus the zone resolution `timestamptz` does
/// on top — the type whose every value carries a zone token.
fn timestamptz_in(c: &mut Criterion) {
    let mut g = c.benchmark_group("timestamptz_in");
    let fmt = FmtCtx::utc(0);

    g.bench_function("iso_offset", |b| {
        b.iter(|| timestamptz::parse(black_box("2013-07-15 10:20:30+00"), &fmt))
    });
    g.bench_function("iso_offset_minutes", |b| {
        b.iter(|| timestamptz::parse(black_box("2013-07-15 10:20:30+05:30"), &fmt))
    });
    // No zone token at all: the session zone answers, and the scan never leaves
    // the fast path.
    g.bench_function("iso_bare", |b| {
        b.iter(|| timestamptz::parse(black_box("2013-07-15 10:20:30"), &fmt))
    });

    g.finish();
}

fn macaddr_in(c: &mut Criterion) {
    let mut g = c.benchmark_group("macaddr_in");

    // PG's spellings, one per grouping rule: six 2-digit groups is what a dump
    // writes, the others have to keep working.
    g.bench_function("six_groups", |b| {
        b.iter(|| macaddr::parse_macaddr(black_box("08:00:2b:01:02:03")))
    });
    g.bench_function("three_groups", |b| {
        b.iter(|| macaddr::parse_macaddr(black_box("0800-2b01-0203")))
    });
    g.bench_function("bare_digits", |b| {
        b.iter(|| macaddr::parse_macaddr(black_box("08002b010203")))
    });
    g.bench_function("macaddr8_eight_bytes", |b| {
        b.iter(|| macaddr::parse_macaddr8(black_box("08:00:2b:01:02:03:04:05")))
    });
    // Six bytes widened to EUI-64, the other length `macaddr8` accepts.
    g.bench_function("macaddr8_from_six", |b| {
        b.iter(|| macaddr::parse_macaddr8(black_box("08:00:2b:01:02:03")))
    });

    g.finish();
}

/// The two `strtoul`-shaped acceptors, whose trim this branch changed.
fn tid_xid_in(c: &mut Criterion) {
    let mut g = c.benchmark_group("tid_xid_in");

    g.bench_function("tid", |b| b.iter(|| tid::parse(black_box("(0,1)"))));
    g.bench_function("xid_small", |b| b.iter(|| xid::xid_in(black_box("42"))));
    // The top of the band, and the negative that wraps into it.
    g.bench_function("xid_max", |b| {
        b.iter(|| xid::xid_in(black_box("4294967295")))
    });
    g.bench_function("xid_wrapping_negative", |b| {
        b.iter(|| xid::xid_in(black_box("-1")))
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
    timestamp_in,
    timestamptz_in,
    macaddr_in,
    tid_xid_in
);
criterion_main!(benches);
