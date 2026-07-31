use crabgresql_types::{Numeric, formatting, formatting_num};
use std::time::Instant;

fn bench(label: &str, n: u64, mut f: impl FnMut() -> usize) {
    let t0 = Instant::now();
    let mut acc = 0usize;
    for _ in 0..n {
        acc += f();
    }
    let e = t0.elapsed();
    println!(
        "{label:55} {:>10.0} ns/call (acc {acc})",
        e.as_nanos() as f64 / n as f64
    );
}

fn main() {
    let n = 1_000_000u64;
    bench("to_char(ts,'YYYY-MM-DD HH24:MI:SS')", n, || {
        formatting::to_char_timestamp(1_000_000, "YYYY-MM-DD HH24:MI:SS")
            .unwrap()
            .unwrap()
            .len()
    });
    bench("to_char(ts, 21 literal chars) [parse only]", n, || {
        formatting::to_char_timestamp(1_000_000, "+++++++++++++++++++++")
            .unwrap()
            .unwrap()
            .len()
    });
    bench("to_char(ts,'+') [1 char]", n, || {
        formatting::to_char_timestamp(1_000_000, "+")
            .unwrap()
            .unwrap()
            .len()
    });
    bench("timestamp::format (hand-rolled baseline)", n, || {
        crabgresql_types::timestamp::format(1_000_000).len()
    });

    let nn = Numeric::parse("12345.678").unwrap();
    bench("to_char(numeric,'999G999D99')", n, || {
        formatting_num::numeric(&nn, "999G999D99").unwrap().len()
    });
    bench("to_char(numeric,'') [empty picture]", n, || {
        formatting_num::numeric(&nn, "").unwrap().len() + 1
    });
    bench("Numeric::to_display (baseline)", n, || {
        nn.to_display().len()
    });
    bench("to_char(float8,'999G999D99')", n, || {
        formatting_num::float8(12345.678, "999G999D99")
            .unwrap()
            .len()
    });
    bench("to_char(int8,'999G999D99')", n, || {
        formatting_num::int8(12345, "999G999D99").unwrap().len()
    });
}
