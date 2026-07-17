//! Vendored fork of the `ryu` crate (v1.0.23) reduced to just the
//! shortest-decimal computation (`d2d`/`f2d`), with one behavioral change:
//! `accept_bounds` is forced to `false` in both `d2s` and `f2s`. This mirrors
//! PostgreSQL's copy of Ryu (`src/common/d2s.c` compiled with
//! `STRICTLY_SHORTEST` disabled), so the shortest digits never land on a
//! float-boundary midpoint and match PG's `float4out`/`float8out` exactly.
//!
//! Upstream: <https://github.com/dtolnay/ryu> / <https://github.com/ulfjack/ryu>.
//! Licensed Apache-2.0 OR BSL-1.0 (see LICENSE-Apache / LICENSE-Boost).

#![allow(clippy::all)]
#![allow(unexpected_cfgs)]
#![allow(dead_code)]

mod common;
pub mod d2s;
mod d2s_full_table;
mod d2s_intrinsics;
pub mod f2s;
mod f2s_intrinsics;

pub use d2s::{DOUBLE_BIAS, DOUBLE_EXPONENT_BITS, DOUBLE_MANTISSA_BITS, FloatingDecimal64, d2d};
pub use f2s::{FLOAT_EXPONENT_BITS, FLOAT_MANTISSA_BITS, FloatingDecimal32, f2d};

/// Shortest-round-trip decimal (mantissa, base-10 exponent) of a finite,
/// nonzero `f64`, using PG's strict-bounds rule. Callers must handle
/// zero/inf/NaN separately.
pub fn shortest_f64(f: f64) -> FloatingDecimal64 {
    let bits = f.to_bits();
    let mantissa = bits & ((1u64 << DOUBLE_MANTISSA_BITS) - 1);
    let exponent = ((bits >> DOUBLE_MANTISSA_BITS) & ((1u64 << DOUBLE_EXPONENT_BITS) - 1)) as u32;
    d2d(mantissa, exponent)
}

/// Shortest-round-trip decimal of a finite, nonzero `f32`.
pub fn shortest_f32(f: f32) -> FloatingDecimal32 {
    let bits = f.to_bits();
    let mantissa = bits & ((1u32 << FLOAT_MANTISSA_BITS) - 1);
    let exponent = (bits >> FLOAT_MANTISSA_BITS) & ((1u32 << FLOAT_EXPONENT_BITS) - 1);
    f2d(mantissa, exponent)
}
