//! Transcript baseline micro-benchmarks. Measures the pre-refactor cost of
//! the three paths the transcript work rewrites: streaming append, full
//! rebuild, and the fold-group scan. Each metric runs across three corpus
//! classes and three sizes so a later run can prove the refactor improved
//! scaling rather than just a point measurement.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

mod support;

use houyicoder_tui::bench_api;

const SIZES: &[usize] = &[1_000, 10_000, 100_000];

fn append_frame_bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("append_frame");
    for &(class, builder) in support::CLASSES {
        for &size in SIZES {
            let id = format!("{class}-{size}");
            group.bench_with_input(BenchmarkId::from_parameter(&id), &size, |b, &n| {
                let mut app = bench_api::app_with_frames(builder(n));
                let extra = builder(n + 1).pop().unwrap();
                b.iter(|| {
                    app.transcript.push_frame(extra.clone());
                    black_box(&app);
                });
            });
        }
    }
    group.finish();
}

fn rebuild_bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("rebuild");
    for &(class, builder) in support::CLASSES {
        for &size in SIZES {
            let id = format!("{class}-{size}");
            group.bench_with_input(BenchmarkId::from_parameter(&id), &size, |b, &n| {
                let frames = builder(n);
                b.iter(|| {
                    let app =
                        bench_api::rebuild_transcript(bench_api::app_with_frames(frames.clone()));
                    black_box(app.transcript.len());
                });
            });
        }
    }
    group.finish();
}

fn fold_scan_bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("fold_scan");
    for &(class, builder) in support::CLASSES {
        for &size in SIZES {
            let id = format!("{class}-{size}");
            group.bench_with_input(BenchmarkId::from_parameter(&id), &size, |b, &n| {
                let app = bench_api::rebuild_transcript(bench_api::app_with_frames(builder(n)));
                b.iter(|| {
                    black_box(bench_api::recompute_fold_group_count(&app));
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, append_frame_bench, rebuild_bench, fold_scan_bench);
criterion_main!(benches);
