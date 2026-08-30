//! Screen resize and soft-wrapped scrollback reflow benchmarks.

mod support;

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use cterm_core::Screen;

use support::{corpus, terminal, COLS, ROWS};

fn populated_screen(data: &[u8], scrollback_lines: usize) -> Screen {
    let (mut screen, mut parser) = terminal(scrollback_lines);
    parser.parse(&mut screen, data);
    screen
}

fn configure_group(group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>) {
    group
        .sample_size(40)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(5));
}

fn resize_viewport(c: &mut Criterion) {
    let data = corpus::filled_viewport();
    let mut group = c.benchmark_group("screen_resize");
    configure_group(&mut group);

    group.bench_function("filled_viewport_80x24_to_120x40", |b| {
        b.iter_batched(
            || populated_screen(&data, 0),
            |mut screen| {
                screen.resize(120, 40);
                screen.resize(COLS, ROWS);
                black_box((screen.width(), screen.height(), screen.cursor.row));
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn reflow_scrollback(c: &mut Criterion) {
    const SCROLLBACK_LINES: usize = 10_000;

    let ascii = corpus::wrapped_ascii();
    let unicode = corpus::wrapped_unicode();
    let mut group = c.benchmark_group("screen_reflow");
    configure_group(&mut group);

    group.bench_function("wrapped_ascii_scrollback", |b| {
        b.iter_batched(
            || populated_screen(&ascii, SCROLLBACK_LINES),
            |mut screen| {
                screen.resize(120, 40);
                screen.resize(60, ROWS);
                screen.resize(COLS, ROWS);
                black_box((screen.total_lines(), screen.cursor.row, screen.cursor.col));
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("wrapped_unicode_grapheme_scrollback", |b| {
        b.iter_batched(
            || populated_screen(&unicode, SCROLLBACK_LINES),
            |mut screen| {
                screen.resize(120, 40);
                screen.resize(60, ROWS);
                screen.resize(COLS, ROWS);
                black_box((screen.total_lines(), screen.cursor.row, screen.cursor.col));
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

criterion_group!(benches, resize_viewport, reflow_scrollback);
criterion_main!(benches);
