//! Frame render timing workload. Ignored by default; run it via
//! make benchmark tui. Raw per-frame durations and percentile summaries
//! land under target/measurements/tui.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::{Terminal, backend::TestBackend};

use crate::records::{ToolOutcome, TranscriptLine};
use crate::state::App;

const SIZES: &[(u16, u16)] = &[(40, 16), (80, 24), (160, 48)];
const SCALES: &[usize] = &[100, 1000, 10000];
const WARMUP: usize = 50;
const SAMPLES: usize = 300;

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }
}

fn word(lcg: &mut Lcg, len: usize) -> String {
    (0..len)
        .map(|_| char::from(b'a' + (lcg.next() % 26) as u8))
        .collect()
}

fn synth_lines(n: usize, seed: u64) -> Vec<TranscriptLine> {
    let mut lcg = Lcg(seed);
    let mut lines = Vec::with_capacity(n);
    for i in 0..n {
        match lcg.next() % 10 {
            0..=1 => lines.push(TranscriptLine::User(format!(
                "user task {}: {} {} {}",
                i,
                word(&mut lcg, 8),
                word(&mut lcg, 6),
                word(&mut lcg, 10)
            ))),
            2..=4 => lines.push(TranscriptLine::Agent(format!(
                "answer {}: {} {} {} {}",
                i,
                word(&mut lcg, 12),
                word(&mut lcg, 8),
                word(&mut lcg, 14),
                word(&mut lcg, 6)
            ))),
            5 => lines.push(TranscriptLine::Thinking {
                text: format!("reasoning {}: {}", i, word(&mut lcg, 20)),
            }),
            6..=7 => lines.push(TranscriptLine::Tool {
                name: "Read".into(),
                tool: "read".into(),
                status: word(&mut lcg, 5),
                invocation: format!("src/{}.rs", word(&mut lcg, 6)),
                outcome: if lcg.next().is_multiple_of(4) {
                    ToolOutcome::Error
                } else {
                    ToolOutcome::Success
                },
                call_id: format!("call-{i}"),
                body: format!(
                    "{} {}\n{}",
                    word(&mut lcg, 10),
                    word(&mut lcg, 30),
                    word(&mut lcg, 20)
                ),
                is_diff: lcg.next().is_multiple_of(3),
            }),
            _ => lines.push(TranscriptLine::System(format!(
                "system {}: {}",
                i,
                word(&mut lcg, 10)
            ))),
        }
    }
    lines
}

fn corpus_hash() -> String {
    // FNV-1a over the rendered parameters: stable across toolchains, unlike
    // the std default hasher, so the corpus id keeps comparing across runs.
    // Both constants are the FNV-1a 64 spec's offset basis and prime.
    let text = format!("{SIZES:?}{SCALES:?}{WARMUP}{SAMPLES}");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn percentile(samples: &[Duration], pct: f64) -> Duration {
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort();
    let idx = ((sorted.len() as f64 - 1.0) * pct).round() as usize;
    sorted[idx]
}

fn git_sha() -> String {
    std::env::var("HOUYI_GIT_SHA").unwrap_or_else(|_| "unknown".into())
}

fn run_id() -> String {
    // The dispatcher passes a readable date-sha id; a bare cargo run falls
    // back to the epoch second so repeat runs never overwrite each other.
    std::env::var("HOUYI_BENCH_RUN_ID").unwrap_or_else(|_| {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        format!("{ts}-unknown")
    })
}

fn render(app: &App, area: (u16, u16)) -> Duration {
    let backend = TestBackend::new(area.0, area.1);
    let mut term = Terminal::new(backend).expect("backend");
    let start = Instant::now();
    crate::view::draw(&mut term.get_frame(), app);
    start.elapsed()
}

fn record(
    run_dir: &std::path::Path,
    scenario: &str,
    area: (u16, u16),
    scale: usize,
    durations: &[Duration],
    rebuilds: u64,
) {
    let p50 = percentile(durations, 0.50);
    let p95 = percentile(durations, 0.95);
    let p99 = percentile(durations, 0.99);
    let max = durations.iter().copied().max().unwrap_or_default();
    println!(
        "{scenario:10} {area:?} scale={scale:5} n={} p50={p50:?} p95={p95:?} p99={p99:?} max={max:?} rebuilds={rebuilds}",
        durations.len()
    );
    let stem = format!("{scenario}-{}x{}-{}", area.0, area.1, scale);
    let raw: String = durations
        .iter()
        .map(|d| format!("{}\n", d.as_micros()))
        .collect();
    std::fs::write(run_dir.join(format!("{stem}.durations_us.txt")), raw)
        .expect("write raw durations");
    let row = format!(
        "{scenario},{},{},{},{},{},{},{},{},{}\n",
        area.0,
        area.1,
        scale,
        durations.len(),
        p50.as_micros(),
        p95.as_micros(),
        p99.as_micros(),
        max.as_micros(),
        rebuilds
    );
    std::fs::write(run_dir.join(format!("{stem}.csv")), row).expect("write summary");
}

/// Counts frames that rebuilt the transcript row cache. The version cell
/// holds a content hash, not a counter, so rebuilds are observed as hash
/// changes between frames.
struct RebuildWatch {
    prev: u64,
    count: u64,
}

impl RebuildWatch {
    fn new(app: &App) -> Self {
        Self {
            prev: app.display_rows_version.get(),
            count: 0,
        }
    }

    fn after_frame(&mut self, app: &App) {
        let v = app.display_rows_version.get();
        if v != self.prev {
            self.count += 1;
            self.prev = v;
        }
    }
}

#[test]
#[ignore = "measurement workload; run via make benchmark tui"]
#[expect(clippy::too_many_lines, reason = "flat sweep is easier to audit")]
fn test_frame_timing_benchmark() {
    let sha = git_sha();
    let run_dir = std::path::Path::new("target/measurements/tui").join(run_id());
    std::fs::create_dir_all(&run_dir).expect("measurements dir");
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    std::fs::write(
        run_dir.join("meta.txt"),
        format!(
            "sha={sha}\nprofile={profile}\nos={}\ncpu={}\ncorpus_hash={}\nsizes={SIZES:?}\nscales={SCALES:?}\nwarmup={WARMUP}\nsamples={SAMPLES}\n",
            std::env::consts::OS,
            std::env::consts::ARCH,
            corpus_hash()
        ),
    )
    .expect("meta");

    for &scale in SCALES {
        let lines = synth_lines(scale, 0xC0FFEE);
        for &area in SIZES {
            // Idle: redraw the same history; the cache should not rebuild.
            let mut app = crate::composition::app();
            app.screen = crate::state::Screen::Working;
            app.transcript = lines.clone().into();
            app.transcript.bump_revision();
            render(&app, area);
            let mut durations = Vec::with_capacity(SAMPLES);
            for _ in 0..WARMUP {
                render(&app, area);
            }
            let mut watch = RebuildWatch::new(&app);
            for _ in 0..SAMPLES {
                durations.push(render(&app, area));
                watch.after_frame(&app);
            }
            record(&run_dir, "idle", area, scale, &durations, watch.count);

            // Streaming at realistic token rates.
            for rate in [100usize, 500, 2000] {
                let per_frame = (rate / 60).max(1);
                let mut app = crate::composition::app();
                app.screen = crate::state::Screen::Working;
                app.transcript = lines.clone().into();
                app.transcript.bump_revision();
                let mut lcg = Lcg(0xFEED + rate as u64);
                render(&app, area);
                let mut watch = RebuildWatch::new(&app);
                let mut durations = Vec::with_capacity(100);
                for i in 0..100 {
                    for t in 0..per_frame {
                        app.transcript.push(TranscriptLine::Agent(format!(
                            "tok {i}-{t}: {} {}",
                            word(&mut lcg, 8),
                            word(&mut lcg, 12)
                        )));
                    }
                    app.transcript.bump_revision();
                    durations.push(render(&app, area));
                    watch.after_frame(&app);
                }
                record(
                    &run_dir,
                    &format!("stream@{rate}tokps"),
                    area,
                    scale,
                    &durations,
                    watch.count,
                );
            }

            // Search: a frozen snapshot view.
            let mut app = crate::composition::app();
            app.screen = crate::state::Screen::Working;
            app.transcript = lines.clone().into();
            app.search_transcript = lines.clone();
            app.transcript.bump_revision();
            render(&app, area);
            let mut durations = Vec::with_capacity(100);
            for _ in 0..WARMUP.min(10) {
                render(&app, area);
            }
            let mut watch = RebuildWatch::new(&app);
            for _ in 0..100 {
                durations.push(render(&app, area));
                watch.after_frame(&app);
            }
            record(&run_dir, "search", area, scale, &durations, watch.count);
        }
    }

    // Startup: time App construction over a synthetic history plus the first
    // frame. Disk load and service spawn are out of scope here.
    for &scale in SCALES {
        let lines = synth_lines(scale, 0xBEEF);
        let mut durations = Vec::with_capacity(20);
        let mut rebuilds = 0u64;
        for _ in 0..20 {
            let start = Instant::now();
            let mut app = crate::composition::app();
            app.screen = crate::state::Screen::Working;
            app.transcript = lines.clone().into();
            app.transcript.bump_revision();
            let mut watch = RebuildWatch::new(&app);
            render(&app, (80, 24));
            watch.after_frame(&app);
            rebuilds += watch.count;
            durations.push(start.elapsed());
        }
        record(&run_dir, "startup", (80, 24), scale, &durations, rebuilds);
    }

    // Resize: cycling sizes. Width is part of the cache key, so each frame
    // invalidates on its own; the version bump keeps the loop uniform with
    // the streaming scenarios.
    let lines = synth_lines(1000, 0x5EED);
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript = lines.clone().into();
    app.transcript.bump_revision();
    render(&app, (80, 24));
    let mut watch = RebuildWatch::new(&app);
    let mut durations = Vec::with_capacity(150);
    for i in 0..150 {
        let area = SIZES[i % SIZES.len()];
        app.transcript.bump_revision();
        durations.push(render(&app, area));
        watch.after_frame(&app);
    }
    record(&run_dir, "resize", (80, 24), 1000, &durations, watch.count);

    // Fold: toggling an expanded tool-result group between frames.
    let mut watch = RebuildWatch::new(&app);
    let mut durations = Vec::with_capacity(100);
    for i in 0..100usize {
        // Even frames expand a group, the next odd frame collapses the same
        // one, so the expanded set truly toggles instead of only growing.
        let call_id = format!("call-{}", 6 + ((i / 2) % 200));
        if i.is_multiple_of(2) {
            app.expanded_results.insert(call_id);
        } else {
            app.expanded_results.remove(&call_id);
        }
        app.transcript.bump_revision();
        durations.push(render(&app, (80, 24)));
        watch.after_frame(&app);
    }
    record(&run_dir, "fold", (80, 24), 1000, &durations, watch.count);

    // Parent/child: a teammate view with its own child transcript.
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.teammate_view = Some(crate::records::TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    app.teammate_view.as_mut().unwrap().transcript = synth_lines(1000, 0xFADE);
    app.transcript = synth_lines(1000, 0xC0FFEE).into();
    app.transcript.bump_revision();
    render(&app, (80, 24));
    let mut watch = RebuildWatch::new(&app);
    let mut durations = Vec::with_capacity(100);
    for _ in 0..100 {
        durations.push(render(&app, (80, 24)));
        watch.after_frame(&app);
    }
    record(
        &run_dir,
        "parent-child",
        (80, 24),
        1000,
        &durations,
        watch.count,
    );

    println!("raw samples: {}", run_dir.display());
}
