//! Measurement workload for the history read path, driven through the real
//! binary in a PTY over a large on-disk session log. Ignored by default; run
//! via the history benchmark target, which builds the release binary first
//! and stamps the git sha and run id into the environment.
//! Samples land under target/measurements/history-pgup, mirroring the frame
//! timing workload: meta.txt, one csv per sample class, and a summary the
//! test also prints.

// The shared harness serves every PTY binary; this one only drives the
// session, the large-fixture seeder, and the temp-dir creator, so the rest of
// the module is legitimately unused here.
#[allow(dead_code)]
mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{
    Key, LargeFixture, PtySession, fresh_temp_dir, launch_and_settle, sample_rss_kb,
    seed_large_session_on_disk, sid_dirs, top_row, view_line, view_total,
};
use std::env::consts::ARCH;
use std::env::consts::OS;
use std::env::var;
use std::fs::File;
use std::fs::create_dir_all;
use std::fs::metadata;
use std::fs::write;
use std::io::BufRead;
use std::io::BufReader;
use std::process::id;
use std::str::from_utf8;
use std::thread::sleep;

/// Mirrors RESIDENT_BYTE_BUDGET in the transcript state: frames newer than
/// this many bytes from the tail stay in memory; anything older is reachable
/// only through a history read. Kept as a copy because the tui constant is
/// crate-private; a drift shows up as episodes landing on the wrong side of
/// the boundary, not as a wrong number.
const RESIDENT_BUDGET_BYTES: u64 = 8 * 1024 * 1024;

/// The fixture size for the page-up baseline. The baseline run scaled this
/// down from 64 MiB because a replay that size did not settle within 300 s;
/// the descent shape needs a log past the 8 MiB resident boundary, which 16
/// MiB gives with roughly thirty read windows and both cold and warm
/// episodes. The size is held here rather than raised so the episodes stay
/// comparable across runs.
const PGUP_FIXTURE_BYTES: u64 = 16 * 1024 * 1024;

/// The fixture sizes for the idle-memory comparison. The baseline could
/// measure two points either side of the resident boundary because replay
/// time ruled out anything larger; the four points here span 25x the log, so
/// the retention reads as a slope rather than a single ratio.
const RSS_FIXTURE_BYTES: &[u64] = &[
    4 * 1024 * 1024,
    16 * 1024 * 1024,
    64 * 1024 * 1024,
    100 * 1024 * 1024,
];

/// How long one press waits for the top row to change before it counts as a
/// stall press. A change means rows were already loaded; a stall means the
/// view hit the top of the loaded rows and a history read is now pending.
const CHANGE_WINDOW: Duration = Duration::from_millis(150);

/// Poll interval of the continuous observation loop. The screen is read on
/// every tick whether or not a press is pending, so a late history read
/// landing between two presses cannot hide a screen state from the scan.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// A press is sent only after the top row has held still for this long.
/// Sending while the view is moving queues the key, and queued keys are
/// consumed together once the blocking work finishes, folding several pages
/// into one observed change and losing every screen state in between.
const STABLE_WINDOW: Duration = Duration::from_millis(30);

/// Hard cap on presses. The 16 MiB fixture renders about 30,000 rows and a
/// page moves about a viewport, so a full descent plus the probe presses
/// that accompany stall episodes stays under this cap.
const MAX_PRESSES: usize = 1400;

/// Frozen-view watchdog. From the episode's first stall the descent allows
/// this long for a history read to land (the status row count grows) or the
/// view to move; probe presses keep flowing meanwhile but do not reset the
/// clock. A full window with no landing means the disk rows are exhausted.
/// Ten seconds is over ten times the slowest landing observed at the
/// baseline commit.
const LANDING_TIMEOUT: Duration = Duration::from_secs(10);

/// The marker step planted by the large-fixture seeder, recorded into the
/// run's meta so a reader does not have to open the seeder to know it.
const MARKER_STEP_BYTES: u64 = 64 * 1024;

/// Sessions root holding a provider transcript converted with the workspace
/// loader. Unset means the real-transcript run is skipped: the input is a
/// user's own session, so it is supplied from outside the repository.
const MEASURE_SESSION_DIR: &str = "HOUYICODER_MEASURE_SESSION_DIR";

/// The alternate-screen entry, the terminal's own signal that the interface is
/// up. The prompt placeholder cannot carry this measurement: it also occurs in
/// the transcript's own rows, so a match there would time text arriving rather
/// than the first frame.
const FIRST_PAINT: &str = "\u{1b}[?1049h";

/// Safety bound on the page-up walk. The walk ends on its own signal, on the
/// press that stops moving the view, and on a long session that is where the
/// pane's scrollback ends rather than the session's opening row; this only caps
/// a run against a view that never stops.
const CONVERTED_PRESSES: usize = 8000;

/// How many history reads the run prints. The full series lands in the press
/// sample beside it; the console carries the shape, not every row.
const LANDINGS_PRINTED: usize = 6;

/// How many rows the view comes to rest on that the run prints. The head of the
/// at-rest view separates the two ways a walk ends: the log's own opening rows,
/// or a row standing in for content the pane could not load.
const ROWS_AT_REST: usize = 4;

/// Response window for one page-up over the real transcript. A cold read of a
/// log this size takes longer than the tight window the seeded baseline uses,
/// so the window is wide enough to hold a sample read. It is a window, not a
/// deadline, and the walk cannot tell a read slower than it from no read at
/// all: on a log past the resident budget both end the walk with no landing,
/// which the landing gate then reports as a missing read. The distinction the
/// walk does keep is the extent, so a run that reddens the gate is re-read from
/// the press rows before the read path is called broken.
const CONVERTED_CHANGE_WINDOW: Duration = Duration::from_secs(5);

/// Replay watchdog for the real transcript. It is a log no seeder produced, so
/// the timeout covers a cold disk read of the whole file; a resume that misses
/// this window has stalled rather than loaded slowly, and the run should report
/// that instead of waiting on it.
const REPLAY_TIMEOUT: Duration = Duration::from_secs(60);

fn git_sha() -> String {
    var("HOUYI_GIT_SHA").unwrap_or_else(|_| "unknown".into())
}

fn run_id() -> String {
    var("HOUYI_BENCH_RUN_ID").unwrap_or_else(|_| format!("adhoc-{}", id()))
}

fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

fn history_run_dir() -> PathBuf {
    let dir = Path::new("target/measurements/history-pgup").join(run_id());
    create_dir_all(&dir).expect("measurements dir");
    dir
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn opt_text(value: Option<u64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "na".into())
}

/// A timed press's latency to one decimal, so the column keeps the fraction of
/// a millisecond the fastest presses land in. Absent reads the same as an
/// absent screen reading.
fn opt_ms(value: Option<f64>) -> String {
    value
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "na".into())
}

/// The marker token layout: a head literal, four decimal digits identifying
/// the marker, then a tail literal.
const MARKER_HEAD: &str = "MK";
const MARKER_TAIL: &str = "QZX";
const MARKER_DIGITS: usize = 4;

/// The marker index if a complete marker token starts at this byte offset.
/// Works on bytes so the parse never slices the screen text at a non-ASCII
/// boundary.
fn marker_index_at(contents: &str, start: usize) -> Option<usize> {
    let digits_at = start + MARKER_HEAD.len();
    let digits_end = digits_at + MARKER_DIGITS;
    let token_end = digits_end + MARKER_TAIL.len();
    let bytes = contents.as_bytes();
    if bytes.len() < token_end {
        return None;
    }
    let digits = &bytes[digits_at..digits_end];
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if &bytes[digits_end..token_end] != MARKER_TAIL.as_bytes() {
        return None;
    }
    from_utf8(digits).ok()?.parse::<usize>().ok()
}

/// A marker is disk-zone when its event line sits older than the resident
/// budget from the tail, so seeing it proves a history read landed.
fn marker_is_disk(fixture: &LargeFixture, idx: usize) -> bool {
    let boundary = fixture.log_bytes.saturating_sub(RESIDENT_BUDGET_BYTES);
    fixture.markers[idx].byte_depth < boundary
}

/// Tracks planted markers against the live screen. Every poll scans the whole
/// screen for the marker token format in one pass. An earlier design walked a
/// small window in discovery order and deadlocked on the first miss: the
/// cursor could not advance past a marker that was never seen, so everything
/// beyond the window went unscanned for the rest of the descent.
struct MarkerScanner<'a> {
    fixture: &'a LargeFixture,
    seen: Vec<bool>,
    /// Marker index to the press that first saw it.
    first_seen: HashMap<usize, usize>,
}

impl<'a> MarkerScanner<'a> {
    fn new(fixture: &'a LargeFixture) -> Self {
        Self {
            fixture,
            seen: vec![false; fixture.markers.len()],
            first_seen: HashMap::new(),
        }
    }

    /// Return the marker indices that appear on this screen for the first
    /// time, attributed to one press.
    fn scan(&mut self, contents: &str, press: usize) -> Vec<usize> {
        let mut found = Vec::new();
        for start in contents.match_indices(MARKER_HEAD).map(|(at, _)| at) {
            let Some(idx) = marker_index_at(contents, start) else {
                continue;
            };
            if idx < self.seen.len() && !self.seen[idx] {
                self.seen[idx] = true;
                self.first_seen.insert(idx, press);
                found.push(idx);
            }
        }
        found
    }

    fn seen_count(&self) -> usize {
        self.seen.iter().filter(|s| **s).count()
    }

    fn disk_seen_count(&self) -> usize {
        self.first_seen
            .keys()
            .filter(|idx| marker_is_disk(self.fixture, **idx))
            .count()
    }

    /// True when the marker nearest the log's head has been seen: the descent
    /// then covered the whole log, not just part of the disk zone.
    fn saw_oldest(&self) -> bool {
        self.seen.first().copied().unwrap_or(false)
    }
}

/// One page-up press, as observed from outside the process.
struct PressRecord {
    idx: usize,
    /// Send time relative to the loop start, in milliseconds.
    t_send_ms: f64,
    /// Time from send to the first top-row change, if one happened inside
    /// the press's change window.
    change_us: Option<u64>,
    stall: bool,
    markers_new: usize,
    markers_disk_new: usize,
}

/// One frozen-view span: it opens when a press stalls and closes when the top
/// row moves again, whether a later press moved it or a slow rebuild finished
/// on its own. The wall span runs from the first stalled press's send to the
/// observed movement: that is how long the user waits between the view
/// stopping and the view moving again.
struct Episode {
    stall_presses: usize,
    wall_ms: f64,
    close_press: usize,
    markers_new: usize,
    markers_disk_new: usize,
    /// Press send to observed row-count growth, when a landing was observed.
    /// Separates a real history read from a slow rebuild that merely held the
    /// view frozen past the change window.
    landing_ms: Option<u64>,
}

/// One press that was sent and has not resolved yet. It resolves as changed
/// once the top row moved and then held still, or as stalled once the change
/// window expired with the view frozen.
struct PendingPress {
    idx: usize,
    t_send: Instant,
    /// The instant the top row first moved after the send, if it did. The
    /// change latency and every episode wall span derive from this instant,
    /// so no span can pick up time spent waiting for the press to resolve.
    t_change: Option<Instant>,
    markers_new: usize,
    markers_disk_new: usize,
}

/// The watch opened by a stall press. The view sits frozen at the top of the
/// loaded rows and a history read may be pending. The landing is observed on
/// the status row count, which grows when rows prepend above a held viewport
/// even though the visible content does not move. Probe presses keep the
/// input ticks flowing while the watch is open, because the baseline commit
/// dispatches and draws history reads only on input ticks.
struct LandingWatch {
    /// Row count total when the watch opened or last re-stalled, or None
    /// when the status line did not parse; landing and exhaustion detection
    /// are then unavailable and the press cap bounds the descent.
    total_at_stall: Option<u64>,
    /// Send instant of the episode's first stall press: the request instant
    /// for landing latency.
    stall_send: Instant,
    /// When the watch first opened. Not rebased on re-stall, so the
    /// exhaustion watchdog measures the whole episode.
    since: Instant,
    landed: bool,
    landing_ms: Option<u64>,
    /// Last probe press sent or scheduled, and whether one is due now.
    last_probe: Instant,
    probe_due: bool,
}

/// What the caller knows about the movement that ended an episode.
struct EpisodeClose {
    close_press: usize,
    /// The instant the closing movement was observed. The wall span runs
    /// from the first stalled press's send to here, so it ends at the
    /// movement itself, not at the press resolve that follows it.
    wall_end: Instant,
    markers_new: usize,
    markers_disk_new: usize,
    landing_ms: Option<u64>,
    /// True when the view moved on its own, with no closing press.
    spontaneous: bool,
}

/// Why the observation loop stopped.
#[derive(Clone, Copy)]
enum DescentExit {
    /// The frozen-view watchdog expired with no landing: the disk rows are
    /// exhausted, or the read path stopped answering.
    Watchdog,
    /// The press cap was reached first.
    PressCap,
}

impl DescentExit {
    fn as_text(self) -> &'static str {
        match self {
            DescentExit::Watchdog => "watchdog",
            DescentExit::PressCap => "press_cap",
        }
    }
}

/// The continuous observation loop driving the descent. Every tick polls the
/// screen, and every send is gated on the top row having held still, so keys
/// never queue up and no page can slip by unobserved. While a watch is open
/// the sends continue as periodic probes: the baseline commit needs input
/// ticks to dispatch and draw history reads, so withholding keys would turn
/// a live read into an endless freeze.
struct Descent<'s, 'f> {
    session: &'s mut PtySession,
    scanner: MarkerScanner<'f>,
    loop_start: Instant,
    presses: Vec<PressRecord>,
    episodes: Vec<Episode>,
    pending: Option<PendingPress>,
    watch: Option<LandingWatch>,
    /// The open episode's first stall: its press index and send instant.
    open_stall: Option<(usize, Instant)>,
    last_top: String,
    last_move: Instant,
    last_total: Option<u64>,
    /// Last instant the status row count grew.
    last_growth: Option<Instant>,
    /// Markers seen with no pending press to attribute them to, flushed into
    /// the episode a spontaneous movement closes or into the last press.
    spill_new: usize,
    spill_disk: usize,
    done: bool,
}

impl<'s, 'f> Descent<'s, 'f> {
    fn new(session: &'s mut PtySession, scanner: MarkerScanner<'f>) -> Self {
        let contents = session.screen().contents();
        let now = Instant::now();
        Self {
            session,
            scanner,
            loop_start: now,
            presses: Vec::new(),
            episodes: Vec::new(),
            pending: None,
            watch: None,
            open_stall: None,
            last_top: top_row(&contents).to_string(),
            last_move: now,
            last_total: view_total(&contents),
            last_growth: None,
            spill_new: 0,
            spill_disk: 0,
            done: false,
        }
    }

    /// Drive the descent until the disk row budget is exhausted, the watchdog
    /// expires, or the press cap lands. Hands back the samples, the scanner,
    /// and why the loop stopped.
    fn run(
        mut self,
    ) -> (
        Vec<PressRecord>,
        Vec<Episode>,
        MarkerScanner<'f>,
        DescentExit,
    ) {
        while !self.done && self.presses.len() < MAX_PRESSES {
            let contents = self.session.screen().contents();
            self.observe(&contents);
            if self.watch_update() {
                self.done = true;
            }
            self.resolve();
            self.maybe_send();
            sleep(POLL_INTERVAL);
        }
        self.flush_pending();
        self.flush_spill();
        let exit = if self.done {
            DescentExit::Watchdog
        } else {
            DescentExit::PressCap
        };
        let Self {
            presses,
            episodes,
            scanner,
            ..
        } = self;
        (presses, episodes, scanner, exit)
    }

    /// One poll tick of observation: track the status total, scan markers,
    /// and account a top-row movement.
    fn observe(&mut self, contents: &str) {
        let total = view_total(contents);
        if let (Some(now_total), Some(prev)) = (total, self.last_total)
            && now_total > prev
        {
            self.last_growth = Some(Instant::now());
        }
        self.last_total = total;

        let attribution = self
            .pending
            .as_ref()
            .map(|p| p.idx)
            .or_else(|| self.presses.last().map(|p| p.idx))
            .unwrap_or(0);
        for idx in self.scanner.scan(contents, attribution) {
            let disk = marker_is_disk(self.scanner.fixture, idx);
            match &mut self.pending {
                Some(p) => {
                    p.markers_new += 1;
                    p.markers_disk_new += usize::from(disk);
                }
                None => {
                    self.spill_new += 1;
                    self.spill_disk += usize::from(disk);
                }
            }
        }

        let top = top_row(contents);
        if top == self.last_top {
            return;
        }
        self.last_top = top.to_string();
        self.last_move = Instant::now();
        if let Some(p) = &mut self.pending {
            if p.t_change.is_none() {
                p.t_change = Some(self.last_move);
            }
            return;
        }
        if self.open_stall.is_some() {
            // The view moved with no press pending: work already running
            // finished and drew on its own, whether a history read landing or
            // a slow rebuild. That movement ends the episode, and the instant
            // it was observed is the wall end.
            let close_press = self.presses.last().map(|p| p.idx).unwrap_or(0);
            let (spill_new, spill_disk) = (self.spill_new, self.spill_disk);
            self.spill_new = 0;
            self.spill_disk = 0;
            let landing_ms = self.watch.take().and_then(|w| w.landing_ms);
            self.close_episode(EpisodeClose {
                close_press,
                wall_end: self.last_move,
                markers_new: spill_new,
                markers_disk_new: spill_disk,
                landing_ms,
                spontaneous: true,
            });
        }
    }

    /// Advance the watch: record a landing on the status row count growth,
    /// keep probe presses on cadence, and run the exhaustion watchdog. Probes
    /// are unconditional because the baseline commit pumps history reads on
    /// input ticks: with no key arriving, a dispatched read neither lands nor
    /// draws, and a passive watch would sit out a live read as a freeze.
    /// Returns true when the watchdog expired: no landing for a full window
    /// since the episode opened means the disk rows are exhausted.
    fn watch_update(&mut self) -> bool {
        let last_total = self.last_total;
        let Some(w) = &mut self.watch else {
            return false;
        };
        if !w.landed {
            let grew =
                matches!((last_total, w.total_at_stall), (Some(now), Some(base)) if now > base);
            if grew {
                w.landed = true;
                w.landing_ms = Some(w.stall_send.elapsed().as_millis() as u64);
            }
        }
        if w.last_probe.elapsed() >= CHANGE_WINDOW {
            w.probe_due = true;
            w.last_probe = Instant::now();
        }
        // Without a parsed row count there is neither a landing signal nor an
        // exhaustion signal; the press cap bounds the descent instead.
        !w.landed && w.total_at_stall.is_some() && w.since.elapsed() >= LANDING_TIMEOUT
    }

    /// Finalize the pending press once its outcome is decided: a change plus
    /// the stability latch, or the expired change window.
    fn resolve(&mut self) {
        let due = match &self.pending {
            None => false,
            Some(p) => match p.t_change {
                Some(_) => {
                    self.last_move.elapsed() >= STABLE_WINDOW
                        || p.t_send.elapsed() >= CHANGE_WINDOW * 2
                    // ^ hard cap so a view that keeps rebuilding cannot hold
                    // the press open forever; the send gate still waits for
                    // stillness before the next press.
                }
                None => p.t_send.elapsed() >= CHANGE_WINDOW,
            },
        };
        if !due {
            return;
        }
        let p = self.pending.take().expect("pending press checked above");
        self.record_press(p);
    }

    /// Fold one press into the sample table, closing its episode when a
    /// changed press ends a stall span. The wall end is the press's own
    /// change instant, so the span never picks up the stability wait that
    /// follows the movement.
    fn record_press(&mut self, p: PendingPress) {
        let stalled = p.t_change.is_none();
        let change_us = p
            .t_change
            .map(|t| t.duration_since(p.t_send).as_micros() as u64);
        if stalled {
            if self.open_stall.is_none() {
                self.open_stall = Some((p.idx, p.t_send));
            }
            self.begin_watch(p.t_send);
        } else if self.open_stall.is_some() {
            let landing_ms = self.watch.take().and_then(|w| w.landing_ms);
            self.close_episode(EpisodeClose {
                close_press: p.idx,
                wall_end: p.t_change.expect("unstalled press saw its change"),
                markers_new: p.markers_new,
                markers_disk_new: p.markers_disk_new,
                landing_ms,
                spontaneous: false,
            });
        }
        self.presses.push(PressRecord {
            idx: p.idx,
            t_send_ms: (p.t_send - self.loop_start).as_secs_f64() * 1000.0,
            change_us,
            stall: stalled,
            markers_new: p.markers_new,
            markers_disk_new: p.markers_disk_new,
        });
    }

    /// Fold a press that never resolved into the samples at loop exit, so
    /// every sent press appears in the table. It goes through the regular
    /// recording path, so a press that saw its change just before the exit
    /// still closes its open episode at that change instant.
    fn flush_pending(&mut self) {
        if let Some(p) = self.pending.take() {
            self.record_press(p);
        }
    }

    /// Open the post-stall watch, or refresh it when a probe stalled in turn.
    /// A re-stall rebases the landing baseline but not the watchdog clock, so
    /// consecutive probe stalls cannot postpone exhaustion forever. A read
    /// that landed inside the stall press's own window is already visible in
    /// the growth instant, so the watch can open with the landing recorded.
    fn begin_watch(&mut self, stall_send: Instant) {
        let last_total = self.last_total;
        let early = self.last_growth.filter(|g| *g >= stall_send);
        if let Some(w) = &mut self.watch {
            w.total_at_stall = last_total;
            w.landed = false;
            if let Some(g) = early {
                w.landed = true;
                w.landing_ms = Some(g.duration_since(w.stall_send).as_millis() as u64);
            }
            w.last_probe = Instant::now();
            return;
        }
        let landing_ms = early.map(|g| g.duration_since(stall_send).as_millis() as u64);
        self.watch = Some(LandingWatch {
            total_at_stall: last_total,
            stall_send,
            since: Instant::now(),
            landed: landing_ms.is_some(),
            landing_ms,
            last_probe: Instant::now(),
            probe_due: false,
        });
    }

    /// Send the next press when none is pending and the view earned it: in
    /// the normal descent once the top row held still, during a watch when a
    /// probe is due and the view is still.
    fn maybe_send(&mut self) {
        if self.done || self.pending.is_some() || self.presses.len() >= MAX_PRESSES {
            return;
        }
        let stable = self.last_move.elapsed() >= STABLE_WINDOW;
        let allowed = match &mut self.watch {
            None => stable,
            Some(w) => {
                if w.probe_due && stable {
                    w.probe_due = false;
                    true
                } else {
                    false
                }
            }
        };
        if !allowed {
            return;
        }
        self.session.send_key(&Key::PageUp);
        self.pending = Some(PendingPress {
            idx: self.presses.len(),
            t_send: Instant::now(),
            t_change: None,
            markers_new: 0,
            markers_disk_new: 0,
        });
    }

    fn close_episode(&mut self, close: EpisodeClose) {
        let Some((first_idx, first_send)) = self.open_stall.take() else {
            return;
        };
        self.episodes.push(Episode {
            stall_presses: close
                .close_press
                .saturating_sub(first_idx)
                .saturating_add(usize::from(close.spontaneous)),
            wall_ms: close.wall_end.duration_since(first_send).as_secs_f64() * 1000.0,
            close_press: close.close_press,
            markers_new: close.markers_new,
            markers_disk_new: close.markers_disk_new,
            landing_ms: close.landing_ms,
        });
    }

    /// Markers seen with no pending press, folded into the last press record
    /// so no observation is lost.
    fn flush_spill(&mut self) {
        if self.spill_new == 0 && self.spill_disk == 0 {
            return;
        }
        if let Some(last) = self.presses.last_mut() {
            last.markers_new += self.spill_new;
            last.markers_disk_new += self.spill_disk;
        }
        self.spill_new = 0;
        self.spill_disk = 0;
    }
}

/// One completed page-up baseline run, holding everything the analysis and
/// the sample files need.
struct PgupRun<'a> {
    fixture: &'a LargeFixture,
    scanner: MarkerScanner<'a>,
    settle_time: Duration,
    rss_at_settle: Option<u64>,
    rss_at_end: Option<u64>,
    /// Status row count right after settle: the size of the resident view
    /// the descent starts from.
    total_at_settle: Option<u64>,
    /// Status row count at the end of the descent: the resident view plus
    /// every row a history read loaded along the way.
    total_at_end: Option<u64>,
    /// Top line number at the end of the descent: one proves the oldest
    /// rows were reached, which the watchdog exit alone cannot tell.
    view_line_end: Option<u64>,
    /// Why the observation loop stopped.
    exit_reason: DescentExit,
    presses: Vec<PressRecord>,
    episodes: Vec<Episode>,
}

impl PgupRun<'_> {
    /// The press that first revealed a disk-zone marker: presses before it
    /// scroll resident rows, presses from it on scroll loaded-from-disk rows.
    fn first_disk_press(&self) -> Option<usize> {
        self.scanner
            .first_seen
            .iter()
            .filter(|(idx, _)| marker_is_disk(self.fixture, **idx))
            .map(|(_, p)| *p)
            .min()
    }

    /// Split stall episodes into the cold read and the warm reads. Every
    /// episode is a history read cycle at the baseline commit: the resident
    /// view is exhausted within the first presses, so the first episode is
    /// the cold read (index built on demand) and the rest are warm (index
    /// built, file handles and page cache hot). The cold label is audited
    /// rather than asserted: the marker scan is a lower bound because batch
    /// rendering skips screens, so the first disk marker observation can lag
    /// the actual boundary crossing by hundreds of presses. What pins the
    /// split is the first episode's closing press matching the resident view
    /// exhaustion point; the summary records it next to the first disk
    /// marker observation so a reader can check the two against each other.
    fn cold_warm_episodes(&self) -> (Option<&Episode>, Vec<&Episode>) {
        let mut iter = self.episodes.iter();
        (iter.next(), iter.collect())
    }

    /// Top-row change latency in microseconds, split into memory-zone and
    /// disk-zone presses at the first disk marker.
    fn change_latency(&self) -> (Vec<f64>, Vec<f64>) {
        let first_disk = self.first_disk_press();
        let mut memory = Vec::new();
        let mut disk = Vec::new();
        for p in &self.presses {
            if let Some(us) = p.change_us {
                if first_disk.is_some_and(|f| p.idx >= f) {
                    disk.push(us as f64);
                } else {
                    memory.push(us as f64);
                }
            }
        }
        memory.sort_by(|a, b| a.partial_cmp(b).unwrap());
        disk.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (memory, disk)
    }

    /// Loose gates: these are path assertions, not performance assertions. A
    /// failure means the descent never crossed into the disk zone, never
    /// stalled, or lost an observation channel, i.e. the read path changed
    /// shape, and the baseline numbers would be meaningless.
    fn check_path_gates(&self, last_screen: &str) {
        let disk_markers_seen = self.scanner.disk_seen_count();
        assert!(
            disk_markers_seen >= 3,
            "only {disk_markers_seen} disk-zone markers seen (need >= 3); the descent never \
             crossed the resident boundary:\n{last_screen}"
        );
        assert!(
            self.episodes.len() >= 2,
            "only {} stall episodes (need >= 2); page-up never stalled on a history read",
            self.episodes.len()
        );
        let landed = self
            .episodes
            .iter()
            .filter(|e| e.landing_ms.is_some())
            .count();
        assert!(
            landed >= 1,
            "no episode observed a landing; the status row count growth that separates a real \
             history read from a slow rebuild may have broken:\n{last_screen}"
        );
    }

    fn summary_text(&self) -> String {
        let (cold, warm) = self.cold_warm_episodes();
        let (memory, disk) = self.change_latency();
        let stall_count = self.presses.iter().filter(|p| p.stall).count();
        let mut warm_walls: Vec<f64> = warm.iter().map(|e| e.wall_ms).collect();
        warm_walls.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let mut summary = String::new();
        summary.push_str(&format!(
            "settle_ms {:.0}\n",
            self.settle_time.as_secs_f64() * 1000.0
        ));
        summary.push_str(&format!(
            "rss_kb_settle {} rss_kb_end {}\n",
            opt_text(self.rss_at_settle),
            opt_text(self.rss_at_end)
        ));
        summary.push_str(&format!(
            "view_total_settle {} view_total_end {}\n",
            opt_text(self.total_at_settle),
            opt_text(self.total_at_end)
        ));
        let landed = self
            .episodes
            .iter()
            .filter(|e| e.landing_ms.is_some())
            .count();
        summary.push_str(&format!(
            "presses {} changed {} stalled {} episodes {} landed {}\n",
            self.presses.len(),
            self.presses.len() - stall_count,
            stall_count,
            self.episodes.len(),
            landed
        ));
        summary.push_str(&format!(
            "markers_seen {} disk_zone {} oldest_seen {}\n",
            self.scanner.seen_count(),
            self.scanner.disk_seen_count(),
            self.scanner.saw_oldest()
        ));
        summary.push_str(&format!(
            "exit_reason {} view_line_end {} first_disk_press {}\n",
            self.exit_reason.as_text(),
            opt_text(self.view_line_end),
            self.first_disk_press()
                .map_or_else(|| "na".to_string(), |p| p.to_string())
        ));
        if let Some(ep) = cold {
            summary.push_str(&format!(
                "cold_episode stall_presses {} wall_ms {:.1} landing_ms {}\n",
                ep.stall_presses,
                ep.wall_ms,
                opt_text(ep.landing_ms)
            ));
        }
        summary.push_str(&format!(
            "warm_episodes {} wall_ms p50 {:.1} p95 {:.1} max {:.1}\n",
            warm.len(),
            percentile(&warm_walls, 0.50),
            percentile(&warm_walls, 0.95),
            warm_walls.last().copied().unwrap_or(f64::NAN)
        ));
        summary.push_str(&format!(
            "change_us_memory p50 {:.0} p95 {:.0} max {:.0} n {}\n",
            percentile(&memory, 0.50),
            percentile(&memory, 0.95),
            memory.last().copied().unwrap_or(f64::NAN),
            memory.len()
        ));
        summary.push_str(&format!(
            "change_us_disk p50 {:.0} p95 {:.0} max {:.0} n {}\n",
            percentile(&disk, 0.50),
            percentile(&disk, 0.95),
            disk.last().copied().unwrap_or(f64::NAN),
            disk.len()
        ));
        summary
    }

    /// Write the run's meta, per-press samples, episode table, and summary.
    fn write_samples(&self, run_dir: &Path) {
        let boundary = self.fixture.log_bytes.saturating_sub(RESIDENT_BUDGET_BYTES);
        let mut meta = String::new();
        meta.push_str(&format!("sha {}\n", git_sha()));
        meta.push_str(&format!("run_id {}\n", run_id()));
        meta.push_str(&format!("profile {}\n", profile()));
        meta.push_str(&format!("os {} {}\n", OS, ARCH));
        meta.push_str(&format!(
            "fixture log_bytes {} event_count {} markers {} marker_step {MARKER_STEP_BYTES}\n",
            self.fixture.log_bytes,
            self.fixture.event_count,
            self.fixture.markers.len(),
        ));
        meta.push_str(&format!("resident_boundary_bytes {boundary}\n"));
        meta.push_str(&format!(
            "windows_ms change {} poll {} stable {} landing_timeout {}\n",
            CHANGE_WINDOW.as_millis(),
            POLL_INTERVAL.as_millis(),
            STABLE_WINDOW.as_millis(),
            LANDING_TIMEOUT.as_millis()
        ));
        meta.push_str(&format!("max_presses {MAX_PRESSES}\n"));
        write(run_dir.join("meta.txt"), meta).expect("write meta");

        let mut csv =
            String::from("press,t_send_ms,change_us,stall,markers_new,markers_disk_new\n");
        for p in &self.presses {
            csv.push_str(&format!(
                "{},{:.1},{},{},{},{}\n",
                p.idx,
                p.t_send_ms,
                p.change_us.map_or_else(String::new, |u| u.to_string()),
                p.stall as u8,
                p.markers_new,
                p.markers_disk_new
            ));
        }
        write(run_dir.join("presses.csv"), csv).expect("write presses");

        let mut ep_csv = String::from(
            "episode,stall_presses,wall_ms,close_press,markers_new,markers_disk_new,landing_ms\n",
        );
        for (i, ep) in self.episodes.iter().enumerate() {
            ep_csv.push_str(&format!(
                "{},{},{:.1},{},{},{},{}\n",
                i,
                ep.stall_presses,
                ep.wall_ms,
                ep.close_press,
                ep.markers_new,
                ep.markers_disk_new,
                ep.landing_ms.map_or_else(String::new, |v| v.to_string())
            ));
        }
        write(run_dir.join("episodes.csv"), ep_csv).expect("write episodes");

        let summary = self.summary_text();
        write(run_dir.join("summary.txt"), &summary).expect("write summary");
        println!("history pgup baseline ({})", run_dir.display());
        print!("{summary}");
    }
}

/// Baseline for the page-up history read: settle time, stall episodes split
/// by cold and warm against the resident boundary, per-press change latency,
/// and resident set before and after the descent.
#[test]
#[ignore = "measurement workload; run via the history benchmark target"]
fn test_pgup_baseline() {
    let dir = fresh_temp_dir("measure-pgup");
    let sid = "66666666-6666-6666-6666-666666666666";
    let fixture = seed_large_session_on_disk(&dir, sid, "measure-model", PGUP_FIXTURE_BYTES);
    let run_dir = history_run_dir();

    let (mut session, settle_time) = launch_and_settle(&dir, &fixture);
    let pid = session.child_pid().expect("child pid");
    let rss_at_settle = sample_rss_kb(pid);
    let total_at_settle = view_total(&session.screen().contents());

    let scanner = MarkerScanner::new(&fixture);
    let (presses, episodes, scanner, exit_reason) = Descent::new(&mut session, scanner).run();
    let rss_at_end = sample_rss_kb(pid);
    let last_screen = session.screen().contents();
    let total_at_end = view_total(&last_screen);
    let view_line_end = view_line(&last_screen);
    drop(session);

    let run = PgupRun {
        fixture: &fixture,
        scanner,
        settle_time,
        rss_at_settle,
        rss_at_end,
        total_at_settle,
        total_at_end,
        view_line_end,
        exit_reason,
        presses,
        episodes,
    };
    // Samples go out before the gates, so a failed run still leaves its full
    // numbers on disk for diagnosis.
    run.write_samples(&run_dir);
    run.check_path_gates(&last_screen);
}

/// Resident set after settle for several log sizes. The ratios and the slope
/// are reported, not asserted: what the process retains across a resume is
/// expected to grow with the log at the current commit, and recording how it
/// grows is the point. What is asserted is that every run settles and reports
/// a live process.
#[test]
#[ignore = "measurement workload; run via the history benchmark target"]
fn test_idle_rss_scaling() {
    let run_dir = history_run_dir();
    let mut rows =
        String::from("log_bytes,event_count,settle_ms,rss_kb_min,rss_kb_med,rss_kb_max\n");
    let mut results: Vec<RssPoint> = Vec::new();

    for (slot, size) in RSS_FIXTURE_BYTES.iter().enumerate() {
        let dir = fresh_temp_dir(&format!("measure-rss-{slot}"));
        let sid = format!("7777777{}-7777-7777-7777-777777777777", slot);
        let fixture = seed_large_session_on_disk(&dir, &sid, "measure-model", *size);
        let (session, settle_time) = launch_and_settle(&dir, &fixture);
        let pid = session.child_pid().expect("child pid");
        let mut samples: Vec<u64> = Vec::new();
        for _ in 0..3 {
            if let Some(kb) = sample_rss_kb(pid) {
                samples.push(kb);
            }
            sleep(Duration::from_millis(250));
        }
        drop(session);
        assert!(!samples.is_empty(), "no rss sample for {size} byte fixture");
        samples.sort_unstable();
        let med = samples[samples.len() / 2];
        results.push(RssPoint {
            log_bytes: fixture.log_bytes,
            event_count: fixture.event_count as u64,
            rss_kb: med,
        });
        rows.push_str(&format!(
            "{},{},{:.0},{},{},{}\n",
            fixture.log_bytes,
            fixture.event_count,
            settle_time.as_secs_f64() * 1000.0,
            samples[0],
            med,
            samples[samples.len() - 1]
        ));
    }

    write(run_dir.join("idle_rss.csv"), &rows).expect("write idle rss");
    println!("idle rss scaling ({})", run_dir.display());
    print!("{rows}");
    print!("{}", rss_steps(&results));
}

/// Resume over a converted provider transcript, then read the history back on
/// demand. The fixture is a real session converted with the workspace loader,
/// so the log carries a history no seeder reproduces. The sessions root comes
/// from the environment because the transcript is user data and cannot live in
/// the repository; without it the run reports that it skipped rather than
/// measuring an empty session.
///
/// Recorded, not asserted: the time to the first frame, the time to the tail of
/// the loaded history, the resident set at settle, and the page-up latency
/// against rows outside the resident budget. A history read on demand is
/// expected to pay a bounded first screen whatever the log size; the resident
/// set says how much of that log the process is still holding.
///
/// The transcript named by the environment must be larger than the pane's
/// resident budget. A smaller one is measured and recorded, but every press
/// then walks rows already resident, so nothing exercises the read path and
/// the run cannot tell a working read from a pane that never reads.
#[test]
#[ignore = "measurement workload; needs a converted transcript in HOUYICODER_MEASURE_SESSION_DIR"]
fn test_resume_converted_transcript() {
    let Ok(root) = var(MEASURE_SESSION_DIR) else {
        println!("{MEASURE_SESSION_DIR} unset: no converted transcript to measure");
        return;
    };
    let root = PathBuf::from(root);
    let Some(log) = converted_log(&root) else {
        panic!("no session log under {}", root.display());
    };
    // The newest message text is the settle point: it is on screen only once
    // the replay has reached the end of the loaded history, which is what
    // separates a loaded history from one still arriving. It is the newest
    // non-empty text because the wait below searches for it, and every screen
    // contains an empty one. The log may end on a tool row after this message,
    // so the settle time is a floor under the last row's arrival.
    let newest = log
        .newest_text
        .as_deref()
        .expect("the log ends with no non-empty message text");
    let started = Instant::now();
    let mut session = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), log.sid.clone()],
        root,
    );
    assert!(
        session.wait_for(FIRST_PAINT, REPLAY_TIMEOUT),
        "the interface never painted over {} bytes:\n{}",
        log.log_bytes,
        session.output()
    );
    let paint_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert!(
        session.wait_for_screen(newest, REPLAY_TIMEOUT),
        "the loaded history's tail never arrived over {} bytes:\n{}",
        log.log_bytes,
        session.output()
    );
    let settle_ms = started.elapsed().as_secs_f64() * 1000.0;
    let rss_kb = session.child_pid().and_then(sample_rss_kb);
    println!(
        "painted at {paint_ms:.0} ms, settled over {} events at {settle_ms:.0} ms, rss {}",
        log.event_count,
        opt_text(rss_kb)
    );

    let walk = page_up_walk(&mut session);
    let scrolled = session.screen().contents();
    let total_lines = view_total(&scrolled);
    drop(session);

    let landings = walk.landings();
    // The history reads are recorded beside their latency: a memory change to
    // the read path shows up here as a count and a range, which the reached
    // extent alone would not separate from a walk that stopped early.
    let row = format!(
        "{},{},{paint_ms:.0},{settle_ms:.0},{},{},{},{},{},{}\n",
        log.log_bytes,
        log.event_count,
        opt_text(rss_kb),
        walk.ms.len(),
        landings.len(),
        opt_ms(walk.fastest()),
        opt_ms(walk.slowest()),
        opt_text(total_lines)
    );
    let csv = format!(
        "log_bytes,event_count,paint_ms,settle_ms,rss_kb,presses,history_reads,press_ms_fastest,press_ms_slowest,view_total\n{row}"
    );
    // Samples land before the gates, so a failed run still leaves its numbers
    // on disk and on stdout for diagnosis.
    let run_dir = history_run_dir();
    write(run_dir.join("converted_transcript.csv"), &csv).expect("write converted transcript");
    write(run_dir.join("converted_presses.csv"), walk.presses_csv())
        .expect("write converted presses");
    println!("converted transcript resume ({})", run_dir.display());
    print!("{csv}");
    println!(
        "page-up walk: {} presses, {} of them history reads, view line at rest {:?}",
        walk.ms.len(),
        landings.len(),
        view_line(&scrolled)
    );
    for (idx, ms, extent) in landings.iter().take(LANDINGS_PRINTED) {
        println!("  press {idx}: {ms:.1} ms, extent now {extent}");
    }
    print_read_range(&landings);
    print_rows_at_rest(&scrolled, ROWS_AT_REST);

    // Liveness, not the numbers: a press that never moves the view means the
    // history stopped answering, and an absent scroll status means the pane
    // never entered the scroll view the presses are meant to reach.
    assert!(
        !walk.ms.is_empty(),
        "no page-up moved the view over {} events",
        log.event_count
    );
    assert!(
        total_lines.is_some(),
        "the scroll status reported no history extent over {} events",
        log.event_count
    );
    // A log past the resident boundary makes a read unavoidable, so a walk of
    // one with no landing timed the resident window instead of the read path
    // this run exists to measure. The walk still moves the view there, which is
    // why the press count alone does not catch it.
    assert!(
        log.log_bytes <= RESIDENT_BUDGET_BYTES || !landings.is_empty(),
        "no history read landed over {} bytes, past the {} byte resident budget",
        log.log_bytes,
        RESIDENT_BUDGET_BYTES
    );
    // The walk ends where the pane's scrollback ends, which for a long session
    // is a bounded run of rows above the resident window rather than the
    // session's opening row; full-history recall is the search view's. What the
    // run has to show is that the walk ends on its own signal, so a pane that
    // never settles is the failure this catches.
    assert!(
        walk.ms.len() < CONVERTED_PRESSES,
        "the page-up walk hit its {CONVERTED_PRESSES} press bound over {} events",
        log.event_count
    );
}

/// One settled run's resident set, kept beside the log it settled over so a
/// step between two runs can be expressed per byte of log and per event.
struct RssPoint {
    log_bytes: u64,
    event_count: u64,
    rss_kb: u64,
}

/// How the resident set moved between neighbouring runs: the ratio, and the
/// growth per thousand events. A retention that holds the log gives a steady
/// step; one that holds a fixed window gives a step that falls toward zero as
/// the logs grow.
fn rss_steps(points: &[RssPoint]) -> String {
    let mut out = String::new();
    for neighbours in points.windows(2) {
        let (a, b) = (&neighbours[0], &neighbours[1]);
        if a.rss_kb == 0 || b.event_count <= a.event_count {
            continue;
        }
        let ratio = b.rss_kb as f64 / a.rss_kb as f64;
        let per_1000 = (b.rss_kb.saturating_sub(a.rss_kb)) as f64 * 1000.0
            / (b.event_count - a.event_count) as f64;
        out.push_str(&format!(
            "rss step {}MiB->{}MiB: ratio {ratio:.2}, {per_1000:.0} KiB per 1000 events\n",
            a.log_bytes / (1024 * 1024),
            b.log_bytes / (1024 * 1024)
        ));
    }
    out
}

/// A converted transcript on disk: the session directory under the configured
/// root that carries a log, with the extent of that log and the text of its
/// newest message row.
struct ConvertedLog {
    sid: String,
    log_bytes: u64,
    event_count: usize,
    newest_text: Option<String>,
}

/// The session under root that carries a log. None when the root holds no
/// session, so a wrong path is reported rather than measured as an empty one.
/// A root holding several sessions stops the run: the numbers are filed under
/// this root, and measuring an arbitrary one of its sessions would file them
/// against the wrong log. The log is read once for its extent and its newest
/// row: the row is the settle latch, and a second pass over hundreds of
/// megabytes to find it would be paid on every run.
fn converted_log(root: &Path) -> Option<ConvertedLog> {
    let carrying: Vec<PathBuf> = sid_dirs(root)
        .into_iter()
        .filter(|path| path.join("log.jsonl").is_file())
        .collect();
    let session_dir = match carrying.as_slice() {
        [] => return None,
        [only] => only.clone(),
        several => panic!("{root:?} holds {} sessions: {several:?}", several.len()),
    };
    let path = session_dir.join("log.jsonl");
    let log_bytes = metadata(&path).ok()?.len();
    let mut event_count = 0usize;
    let mut newest_text = None;
    for line in BufReader::new(File::open(&path).ok()?).lines() {
        event_count += 1;
        let Ok(line) = line else { continue };
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let event = &entry["event"];
        // An empty message text is no latch: the settle wait searches for this
        // snippet, and every screen contains the empty string, so a log whose
        // last message is empty would settle on the first poll.
        if matches!(
            event["type"].as_str(),
            Some("UserInput" | "AssistantMessage")
        ) && let Some(text) = event["text"].as_str()
            && !text.trim().is_empty()
        {
            newest_text = Some(text.chars().take(SNIPPET_CHARS).collect());
        }
    }
    Some(ConvertedLog {
        sid: session_dir.file_name()?.to_string_lossy().into_owned(),
        log_bytes,
        event_count,
        newest_text,
    })
}

/// How many characters of a row's text stand in for the whole row. A short run
/// survives wrapping, so it is still one row on screen.
const SNIPPET_CHARS: usize = 16;

/// Where the scroll view sits, read from the status row: the first visible
/// line and the extent behind it. Both halves are compared because each press
/// outcome moves a different half. A press inside the resident window moves the
/// line; a press past it is answered by a history read, and a read lands by
/// putting rows above a held viewport, so the line stays and the extent grows.
#[derive(PartialEq)]
struct ViewPlace {
    line: Option<u64>,
    extent: Option<u64>,
}

/// Read the view's place off the current screen.
fn view_place(session: &PtySession) -> ViewPlace {
    let screen = session.screen().contents();
    ViewPlace {
        line: view_line(&screen),
        extent: view_total(&screen),
    }
}

/// One page-up against the loaded history: the time from the key press to the
/// view moving. None when neither half moved for the whole window, which is a
/// stall rather than a slow press.
fn press_older(session: &mut PtySession, window: Duration) -> Option<f64> {
    let before = view_place(session);
    let sent = Instant::now();
    session.send_key(&Key::PageUp);
    while sent.elapsed() < window {
        if view_place(session) != before {
            return Some(sent.elapsed().as_secs_f64() * 1000.0);
        }
        sleep(POLL_INTERVAL);
    }
    None
}

/// One completed page-up walk: how long each press took and where the view sat
/// after it. The two series are parallel, so a press's latency and the extent
/// it reached are read together.
struct PressWalk {
    ms: Vec<f64>,
    place: Vec<ViewPlace>,
}

/// Press page-up until a press no longer moves the view, and record each press.
/// The stop has two causes the press count cannot separate: the pane's
/// scrollback ends there, or a read that never lands holds the view where it
/// is. The press bound stops a walk against a view that never stops.
fn page_up_walk(session: &mut PtySession) -> PressWalk {
    let mut walk = PressWalk {
        ms: Vec::new(),
        place: Vec::new(),
    };
    for _ in 0..CONVERTED_PRESSES {
        let Some(ms) = press_older(session, CONVERTED_CHANGE_WINDOW) else {
            break;
        };
        walk.ms.push(ms);
        walk.place.push(view_place(session));
    }
    walk
}

impl PressWalk {
    /// The presses that answered with more rows behind the view than the last
    /// one had. A history read puts older rows above a held viewport, so the
    /// extent is the only half that moves and the only place the read shows up.
    /// Both readings must be present: an extent that was unreadable and then
    /// readable is a parse that started working, not a read that landed.
    fn landings(&self) -> Vec<(usize, f64, u64)> {
        let mut landings = Vec::new();
        for (idx, (ms, place)) in self.ms.iter().zip(&self.place).enumerate() {
            if idx == 0 {
                continue;
            }
            let previous = self.place[idx - 1].extent;
            if let (Some(extent), Some(earlier)) = (place.extent, previous)
                && extent > earlier
            {
                landings.push((idx + 1, *ms, extent));
            }
        }
        landings
    }

    /// The quickest press timed, or None for a walk that timed none. None
    /// rather than a placeholder number: a walk with no press is a run that
    /// failed, and a zero here would read as a press that answered instantly.
    fn fastest(&self) -> Option<f64> {
        self.ms.iter().copied().reduce(f64::min)
    }

    /// The slowest press timed, or None for a walk that timed none.
    fn slowest(&self) -> Option<f64> {
        self.ms.iter().copied().reduce(f64::max)
    }

    /// Every press as a sample row, so the shape of the walk survives the run:
    /// a press inside the resident window and a press that waited on the log
    /// differ by the extent alone.
    fn presses_csv(&self) -> String {
        let mut csv = String::from("press,ms,line,extent\n");
        for (idx, (ms, place)) in self.ms.iter().zip(&self.place).enumerate() {
            csv.push_str(&format!(
                "{},{ms:.1},{},{}\n",
                idx + 1,
                opt_text(place.line),
                opt_text(place.extent)
            ));
        }
        csv
    }
}

/// Print the latency range over the presses that waited on the log. An absent
/// range means no press grew the extent: either the walk stayed inside the
/// resident window, or a read slower than the response window ended it. The run
/// states the absence rather than leaving it to be inferred from an empty
/// range, and the press rows tell the two apart.
fn print_read_range(landings: &[(usize, f64, u64)]) {
    let read_ms: Vec<f64> = landings.iter().map(|(_, ms, _)| *ms).collect();
    if let (Some(slowest), Some(fastest)) = (
        read_ms.iter().copied().reduce(f64::max),
        read_ms.iter().copied().reduce(f64::min),
    ) {
        println!("history read ms: fastest {fastest:.1}, slowest {slowest:.1}");
    }
}

/// Print the rows the view rests on, blank rows skipped. A walk that stops on
/// the log's opening rows and one that stops on a row standing in for content
/// the pane could not load reach the same press count, and only these rows tell
/// them apart.
fn print_rows_at_rest(screen: &str, rows: usize) {
    println!("rows at rest:");
    for line in screen
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(rows)
    {
        println!("  {}", line.trim_end());
    }
}
