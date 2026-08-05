use anyhow::{Result, anyhow};
use crabgresql_types::{Numeric, formatting, formatting_num};
use std::time::Instant;

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

/// `None` is `to_char`'s SQL NULL, which no input here produces.
fn not_null(s: Option<String>) -> Result<String> {
    s.ok_or_else(|| anyhow!("to_char returned NULL"))
}

fn main() -> Result<()> {
    let n = 1_000_000u64;
    bench("to_char(ts,'YYYY-MM-DD HH24:MI:SS')", n, || {
        Ok(not_null(formatting::to_char_timestamp(
            1_000_000,
            "YYYY-MM-DD HH24:MI:SS",
        )?)?
        .len())
    })?;
    bench("to_char(ts, 21 literal chars) [parse only]", n, || {
        Ok(not_null(formatting::to_char_timestamp(
            1_000_000,
            "+++++++++++++++++++++",
        )?)?
        .len())
    })?;
    bench("to_char(ts,'+') [1 char]", n, || {
        Ok(not_null(formatting::to_char_timestamp(1_000_000, "+")?)?.len())
    })?;
    bench("timestamp::format (hand-rolled baseline)", n, || {
        Ok(crabgresql_types::timestamp::format(1_000_000).len())
    })?;

    let nn = Numeric::parse("12345.678")?;
    bench("to_char(numeric,'999G999D99')", n, || {
        Ok(formatting_num::numeric(&nn, "999G999D99")?.len())
    })?;
    bench("to_char(numeric,'') [empty picture]", n, || {
        Ok(formatting_num::numeric(&nn, "")?.len() + 1)
    })?;
    bench("Numeric::to_display (baseline)", n, || {
        Ok(nn.to_display().len())
    })?;
    bench("to_char(float8,'999G999D99')", n, || {
        Ok(formatting_num::float8(12345.678, "999G999D99")?.len())
    })?;
    bench("to_char(int8,'999G999D99')", n, || {
        Ok(formatting_num::int8(12345, "999G999D99")?.len())
    })?;
    Ok(())
}
