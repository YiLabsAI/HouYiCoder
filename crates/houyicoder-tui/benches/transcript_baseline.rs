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

/// The payload-heavy corpus length. The short-payload classes hold far fewer
/// bytes than the ceiling, so only this case makes the byte budget, rather than
/// the frame cap, the bound that the rebuild has to meet.
const LARGE_FRAMES: usize = 2_000;

fn rebuild_bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("rebuild");
    for &(class, builder) in support::CLASSES {
        for &size in SIZES {
            let id = format!("{class}-{size}");
            group.bench_with_input(BenchmarkId::from_parameter(&id), &size, |b, &n| {
                let frames = builder(n);
                // The resident frame log is bounded by its byte budget rather
                // than by the corpus size: a log long enough to exceed the
                // budget drains its oldest frames instead of growing with the
                // session. A rebuild that held every frame would fail here at
                // 100K, which is where the pre-window cost was measured.
                let built =
                    bench_api::rebuild_transcript(bench_api::app_with_frames(frames.clone()));
                let (resident, budget) = bench_api::resident_frame_bytes(&built);
                assert!(
                    resident <= budget as u64,
                    "resident frames fall to the budget at {id}: {resident} > {budget}"
                );
                b.iter(|| {
                    let app =
                        bench_api::rebuild_transcript(bench_api::app_with_frames(frames.clone()));
                    black_box(app.transcript.len());
                });
            });
        }
    }
    // The short-payload classes above never reach the byte ceiling, so their
    // bound is the frame cap. This case exceeds it and holds under the ceiling
    // only by draining, which is the path a payload-heavy session runs.
    group.bench_with_input(
        BenchmarkId::from_parameter("tool-large"),
        &LARGE_FRAMES,
        |b, &n| {
            let frames = support::tool_large(n);
            let built = bench_api::rebuild_transcript(bench_api::app_with_frames(frames.clone()));
            let (resident, budget) = bench_api::resident_frame_bytes(&built);
            let front = bench_api::resident_frame_front(&built);
            assert!(
                resident <= budget as u64,
                "a payload-heavy log falls to the budget: {resident} > {budget}"
            );
            assert!(
                front > 0,
                "the payload-heavy log drains instead of holding every frame"
            );
            b.iter(|| {
                let app = bench_api::rebuild_transcript(bench_api::app_with_frames(frames.clone()));
                black_box(app.transcript.len());
            });
        },
    );
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

/// Quantify the fold cache: the draw path reads the maintained cache (cached),
/// where the pre-cache path recomputed groups on every draw (scan). The two
/// share a rebuilt App so the cache is warm for the cached read and the scan
/// runs over the same lines. The delta is the fold work saved per draw; the
/// cached read stays flat as the session grows while the scan follows the
/// viewable window (capped, so 10K and 100K sit at the same ceiling).
fn fold_access_bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("fold_access");
    let (_, builder) = support::CLASSES
        .iter()
        .copied()
        .find(|(c, _)| *c == "tool")
        .expect("tool class");
    for &size in SIZES {
        let app = bench_api::rebuild_transcript(bench_api::app_with_frames(builder(size)));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("cached-tool-{size}")),
            &size,
            |b, _| {
                b.iter(|| black_box(bench_api::cached_fold_group_count(&app)));
            },
        );
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("scan-tool-{size}")),
            &size,
            |b, _| {
                b.iter(|| black_box(bench_api::recompute_fold_group_count(&app)));
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    append_frame_bench,
    rebuild_bench,
    fold_scan_bench,
    fold_access_bench
);
criterion_main!(benches);
