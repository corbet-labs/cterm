//! Parser throughput across terminal-output workload shapes.

mod support;

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

use support::{corpus, terminal};

fn parse_workload(c: &mut Criterion, name: &str, data: Vec<u8>) {
    let mut group = c.benchmark_group(format!("parser/{name}"));
    group
        .throughput(Throughput::Bytes(data.len() as u64))
        .sample_size(40)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(5));

    group.bench_function("cterm-core", |b| {
        b.iter_batched(
            || terminal(0),
            |(mut screen, mut parser)| {
                parser.parse(&mut screen, black_box(data.as_slice()));
                black_box((screen.cursor.row, screen.cursor.col));
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn parser_benchmarks(c: &mut Criterion) {
    parse_workload(c, "mixed_session", corpus::mixed_session());
    parse_workload(c, "ascii_plain", corpus::ascii_plain());
    parse_workload(c, "sgr_churn", corpus::sgr_churn());
    parse_workload(c, "scroll_storm", corpus::scroll_storm());
    parse_workload(
        c,
        "alternate_screen_redraw",
        corpus::alternate_screen_redraw(),
    );
    parse_workload(
        c,
        "unicode_wide_and_graphemes",
        corpus::unicode_wide_and_graphemes(),
    );
}

criterion_group!(benches, parser_benchmarks);
criterion_main!(benches);
