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

criterion_group!(benches, division, accumulation);
criterion_main!(benches);
