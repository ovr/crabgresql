use anyhow::{Result, anyhow};
use crabgresql_types::{Numeric, formatting, formatting_num};
use std::fmt::Debug;
use std::time::Instant;

/// The crate's error types (`FormatError`, `ParseError`) do not implement
/// `std::error::Error`, so lift them into `anyhow` through their `Debug` form.
fn lift<T, E: Debug>(r: Result<T, E>) -> Result<T> {
    r.map_err(|e| anyhow!("{e:?}"))
}

fn bench(label: &str, n: u64, mut f: impl FnMut() -> Result<usize>) -> Result<()> {
    let t0 = Instant::now();
    let mut acc = 0usize;
    for _ in 0..n {
        acc += f()?;
    }
    let e = t0.elapsed();
    println!(
        "{label:55} {:>10.0} ns/call (acc {acc})",
        e.as_nanos() as f64 / n as f64
    );
    Ok(())
}

fn main() -> Result<()> {
    let n = 1_000_000u64;
    bench("to_char(ts,'YYYY-MM-DD HH24:MI:SS')", n, || {
        Ok(lift(formatting::to_char_timestamp(
            1_000_000,
            "YYYY-MM-DD HH24:MI:SS",
        ))?
        .ok_or_else(|| anyhow!("to_char returned NULL"))?
        .len())
    })?;
    bench("to_char(ts, 21 literal chars) [parse only]", n, || {
        Ok(lift(formatting::to_char_timestamp(
            1_000_000,
            "+++++++++++++++++++++",
        ))?
        .ok_or_else(|| anyhow!("to_char returned NULL"))?
        .len())
    })?;
    bench("to_char(ts,'+') [1 char]", n, || {
        Ok(lift(formatting::to_char_timestamp(1_000_000, "+"))?
            .ok_or_else(|| anyhow!("to_char returned NULL"))?
            .len())
    })?;
    bench("timestamp::format (hand-rolled baseline)", n, || {
        Ok(crabgresql_types::timestamp::format(1_000_000).len())
    })?;

    let nn = lift(Numeric::parse("12345.678"))?;
    bench("to_char(numeric,'999G999D99')", n, || {
        Ok(lift(formatting_num::numeric(&nn, "999G999D99"))?.len())
    })?;
    bench("to_char(numeric,'') [empty picture]", n, || {
        Ok(lift(formatting_num::numeric(&nn, ""))?.len() + 1)
    })?;
    bench("Numeric::to_display (baseline)", n, || {
        Ok(nn.to_display().len())
    })?;
    bench("to_char(float8,'999G999D99')", n, || {
        Ok(lift(formatting_num::float8(12345.678, "999G999D99"))?.len())
    })?;
    bench("to_char(int8,'999G999D99')", n, || {
        Ok(lift(formatting_num::int8(12345, "999G999D99"))?.len())
    })?;
    Ok(())
}
