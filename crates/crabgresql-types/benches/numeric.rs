//! Numeric arithmetic microbenchmarks, weighted toward division.
//!
//! Division is what `avg` pays once per group, so a `GROUP BY` over a
//! near-unique key runs it tens of millions of times. The shapes below are the
//! ones that actually occur: a small `count` divisor (every `avg`), and a
//! divisor too wide for a register (`numeric / numeric` in a query).

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use crabgresql_types::Numeric;

/// `avg`'s exact call: the running sum over the row count, one per group.
fn avg(sum: i128, count: i64) -> Numeric {
    Numeric::from_i128(sum)
        .div(&Numeric::from_i128(count as i128))
        .expect("count is nonzero")
}

fn division(c: &mut Criterion) {
    let mut g = c.benchmark_group("numeric_div");

    // ClickBench q33's shape: a near-unique group key, so every group holds one
    // row and the divisor is 1. `select_div_scale` still asks for 16 fractional
    // digits, so this is not a trivial quotient.
    g.bench_function("avg_singleton_group", |b| {
        b.iter(|| avg(black_box(1913), black_box(1)))
    });

    // A group with many rows: the sum is wider and the divisor is four digits.
    g.bench_function("avg_fat_group", |b| {
        b.iter(|| avg(black_box(2_391_875_000), black_box(2500)))
    });

    // The smoke suite's bigint case, where the quotient's magnitude drives the
    // display scale down to zero.
    g.bench_function("avg_wide_sum", |b| {
        b.iter(|| avg(black_box(27_670_116_110_564_327_421), black_box(3)))
    });

    // A divisor too wide for a register, which has to stay on the schoolbook
    // path with its per-digit big-number temporaries.
    let wide_num = Numeric::parse("123456789012345678901234567890.12345").expect("valid");
    let wide_den = Numeric::parse("98765432109876543210987654321").expect("valid");
    g.bench_function("div_wide_divisor", |b| {
        b.iter(|| {
            black_box(&wide_num)
                .div(black_box(&wide_den))
                .expect("nonzero")
        })
    });

    g.finish();
}

fn accumulation(c: &mut Criterion) {
    let mut g = c.benchmark_group("numeric_accumulate");

    // The per-row cost an aggregate pays to accumulate in `Numeric` rather than
    // a register: `avg`/`sum` over `numeric` do this once per input row, while
    // the integer cases already accumulate in an `i64`/`i128` register, so this
    // is the yardstick for whether a register accumulator is worth it for
    // `numeric` too. Division above is the once-per-group cost; this is the
    // per-row one.
    g.bench_function("add_small", |b| {
        let x = Numeric::from_i128(1913);
        b.iter(|| {
            let mut sum = Numeric::from_i128(0);
            for _ in 0..100 {
                sum = sum.add(black_box(&x));
            }
            sum
        })
    });

    g.bench_function("from_i128", |b| {
        b.iter(|| Numeric::from_i128(black_box(9_223_372_036_854_775_807)))
    });

    g.finish();
}

/// The `Value` ⇄ decimal conversions the columnar stores run **per cell**, one
/// case per real column shape: the cost turns on the magnitude *after* scaling,
/// not on the value the user sees.
///
/// The pair to watch is `bare_*` against `d64_*`. A column with no typmod
/// stores at scale 16, so `321000.00` becomes `3.21e21` — past `u64::MAX`, and
/// so onto 128-bit division — while the same value in `numeric(15,2)` scales to
/// `32100000` and stays in a register.
fn fixed_point(c: &mut Criterion) {
    let mut g = c.benchmark_group("numeric_fixed_point");

    // A `numeric(15,2)` column: the TPC-H money shape, stored as `Decimal64`.
    let money = Numeric::parse("321000.00").expect("valid");
    g.bench_function("d64_to_scaled", |b| {
        b.iter(|| black_box(&money).to_scaled_i128(15, 2))
    });
    g.bench_function("d64_from_scaled", |b| {
        b.iter(|| Numeric::from_scaled_i128(black_box(32_100_000), 2))
    });

    // The same value in a column with no typmod, stored at scale 16.
    g.bench_function("bare_to_scaled", |b| {
        b.iter(|| black_box(&money).to_scaled_i128(38, 16))
    });
    g.bench_function("bare_from_scaled", |b| {
        b.iter(|| Numeric::from_scaled_i128(black_box(3_210_000_000_000_000_000_000), 16))
    });

    // A quotient, which is what an unconstrained column most often holds after
    // arithmetic: sixteen significant digits, so nothing is trailing padding.
    let quotient = Numeric::parse("3.3333333333333333").expect("valid");
    g.bench_function("bare_to_scaled_quotient", |b| {
        b.iter(|| black_box(&quotient).to_scaled_i128(38, 16))
    });
    g.bench_function("bare_from_scaled_quotient", |b| {
        b.iter(|| Numeric::from_scaled_i128(black_box(33_333_333_333_333_333), 16))
    });

    // A `numeric(9,2)` column, stored as `Decimal32` — the narrowest width.
    let small = Numeric::parse("1234.56").expect("valid");
    g.bench_function("d32_to_scaled", |b| {
        b.iter(|| black_box(&small).to_scaled_i128(9, 2))
    });
    g.bench_function("d32_from_scaled", |b| {
        b.iter(|| Numeric::from_scaled_i128(black_box(123_456), 2))
    });

    // A `numeric(76,38)` column, past every Rust integer: the conversion goes
    // through the value's own decimal rendering.
    let wide = Numeric::parse("12345678901234567890123456789012345678.5").expect("valid");
    let mut buffer = String::new();
    g.bench_function("d256_write_scaled", |b| {
        b.iter(|| black_box(&wide).write_scaled_string(76, 38, &mut buffer))
    });

    g.finish();
}

/// Rendering, which every value pays on its way to the client — and a column
/// with no typmod pays most, printing eighteen characters for a value the user
/// wrote as four.
fn rendering(c: &mut Criterion) {
    let mut g = c.benchmark_group("numeric_render");

    let money = Numeric::parse("321000.00").expect("valid");
    g.bench_function("display_d64", |b| b.iter(|| black_box(&money).to_display()));

    // The same value as an unconstrained column stores it.
    let padded = Numeric::parse("321000.0000000000000000").expect("valid");
    g.bench_function("display_bare", |b| {
        b.iter(|| black_box(&padded).to_display())
    });

    // Leading zeros after the point, which is the other side of the same walk.
    let small = Numeric::parse("0.0000000000000015").expect("valid");
    g.bench_function("display_small", |b| {
        b.iter(|| black_box(&small).to_display())
    });

    let whole = Numeric::from_i128(9_223_372_036_854_775_807);
    g.bench_function("display_integer", |b| {
        b.iter(|| black_box(&whole).to_display())
    });

    g.finish();
}

/// What the columnar write gate runs per cell: prove the value fits the
/// column's decimal, then put it in the form the column stores.
fn storage_form(c: &mut Criterion) {
    let mut g = c.benchmark_group("numeric_storage_form");

    // A `numeric(15,2)` value, already at the column's scale — the common case,
    // since `apply_typmod` has been through it.
    let money = Numeric::parse("321000.00").expect("valid");
    g.bench_function("fits_d64", |b| {
        b.iter(|| black_box(&money).fits_decimal(15, 2))
    });
    g.bench_function("trunc_d64", |b| b.iter(|| black_box(&money).trunc(2)));

    // The same value entering a column with no typmod, whose scale it does not
    // already carry.
    g.bench_function("fits_bare", |b| {
        b.iter(|| black_box(&money).fits_decimal(38, 16))
    });
    g.bench_function("trunc_bare", |b| b.iter(|| black_box(&money).trunc(16)));

    g.finish();
}

criterion_group!(
    benches,
    division,
    accumulation,
    fixed_point,
    rendering,
    storage_form
);
criterion_main!(benches);
