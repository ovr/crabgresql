//! `to_char` microbenchmarks, against the hand-rolled formatters they wrap.
//!
//! Each `to_char` case is paired with the baseline it should be compared to —
//! the picture-less formatter for the same type — because the interesting
//! number is the cost of parsing and applying the picture, not the total.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use crabgresql_types::{Numeric, formatting, formatting_num, timestamp};

/// A fixed instant; the value does not matter, only that it is not re-derived.
const TS: i64 = 1_000_000;

fn to_char_timestamp(c: &mut Criterion) {
    let mut g = c.benchmark_group("to_char_timestamp");

    g.bench_function("full_picture", |b| {
        b.iter(|| formatting::to_char_timestamp(black_box(TS), "YYYY-MM-DD HH24:MI:SS"))
    });
    // 21 characters that are all literal: isolates picture parsing from field
    // formatting, since nothing here has to be rendered.
    g.bench_function("literals_only", |b| {
        b.iter(|| formatting::to_char_timestamp(black_box(TS), "+++++++++++++++++++++"))
    });
    g.bench_function("one_char", |b| {
        b.iter(|| formatting::to_char_timestamp(black_box(TS), "+"))
    });
    g.bench_function("baseline_no_picture", |b| {
        b.iter(|| timestamp::format(black_box(TS)))
    });

    g.finish();
}

fn to_char_numbers(c: &mut Criterion) {
    let mut g = c.benchmark_group("to_char_number");
    let value = Numeric::parse("12345.678").expect("valid");

    g.bench_function("numeric_grouped", |b| {
        b.iter(|| formatting_num::numeric(black_box(&value), "999G999D99"))
    });
    g.bench_function("numeric_empty_picture", |b| {
        b.iter(|| formatting_num::numeric(black_box(&value), ""))
    });
    g.bench_function("baseline_to_display", |b| {
        b.iter(|| black_box(&value).to_display())
    });
    g.bench_function("float8_grouped", |b| {
        b.iter(|| formatting_num::float8(black_box(12345.678), "999G999D99"))
    });
    g.bench_function("int8_grouped", |b| {
        b.iter(|| formatting_num::int8(black_box(12345), "999G999D99"))
    });

    g.finish();
}

criterion_group!(benches, to_char_timestamp, to_char_numbers);
criterion_main!(benches);
