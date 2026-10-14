//! On-disk session seeding helpers for the PTY tests: write the session log
//! and descriptor (and a checkpoint manifest for the post-compact journey) into
//! a temp sessions dir the binary will read on --resume. Split from mod.rs on
//! the file-size gate. Each helper is self-contained (external-crate imports
//! only); the read path in LocalFileBackend scans the same layout these write,
//! so the binary's resume + /context see the seeded state.

use futures::executor::block_on;
use houyicoder_context::ContextBackend;
use houyicoder_context::{CheckpointId, CheckpointManifest, Disposition, TurnGroup};
use houyicoder_core::{EventId, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_memory::LocalFileBackend;
use houyicoder_service::composition::workspace_cwd;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::write;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

/// Seed a session on disk (a sid log file with one UserInput event + a
/// session.json descriptor carrying the model) so a subsequent --resume <sid>
/// or the /resume picker can re-open it. Shared by the sid-resume + lock
/// PTY tests.
pub fn seed_session_on_disk(root: &Path, sid_str: &str, model: &str, prompt: &str) {
    let cwd = workspace_cwd(None);
    seed_session_with_cwd(root, sid_str, model, prompt, &cwd);
}

/// Seed a session with an EXPLICIT recorded cwd (not the current workspace).
/// Used by the --continue cwd-convergence test to plant an other-workspace
/// session the cwd filter must reject -- a session that would be picked if
/// the filter were absent, so its presence discriminates the filter from a
/// no-session empty case. Returns the seeded event's id so a caller that
/// also writes a checkpoint manifest (seed_session_with_checkpoint) can
/// reference it as last_event + the Summarized turn without re-deriving or
/// duplicating the log+descriptor write.
pub fn seed_session_with_cwd(
    root: &Path,
    sid_str: &str,
    model: &str,
    prompt: &str,
    cwd: &str,
) -> EventId {
    let sid = SessionId::from_display_string(sid_str).expect("sid parses");
    let event = SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: prompt.into(),
        },
    };
    let line = serde_json::to_string(&event).expect("serialize event");
    let dir = root.join(sid_str);
    create_dir_all(&dir).expect("mkdir session dir");
    write(dir.join("log.jsonl"), format!("{line}\n")).expect("write log");
    let meta = serde_json::json!({
        "name": null,
        "name_source": "auto",
        "cwd": cwd,
        "model": model,
        "provenance": {"kind": "fresh"},
        "version": "test",
        "created_at": 1000,
    });
    write(
        dir.join("session.json"),
        serde_json::to_string_pretty(&meta).expect("serialize meta"),
    )
    .expect("write descriptor");
    event.id
}

/// Seed a session on disk with one user turn per prompt, so a PTY test can
/// drive a multi-turn trajectory list. Writes the log through the real backend
/// so the hash chain is the one a live session produces, then the descriptor.
pub fn seed_session_turns_on_disk(root: &Path, sid_str: &str, model: &str, prompts: &[&str]) {
    let sid = SessionId::from_display_string(sid_str).expect("sid parses");
    let backend = LocalFileBackend::new(root.to_path_buf());
    for (i, prompt) in prompts.iter().enumerate() {
        let event = SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: (i as u64) * 1000,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: (*prompt).into(),
            },
        };
        block_on(backend.append(event)).expect("append seeded turn");
    }
    let cwd = workspace_cwd(None);
    let dir = root.join(sid_str);
    create_dir_all(&dir).expect("mkdir session dir");
    let meta = serde_json::json!({
        "name": null,
        "name_source": "auto",
        "cwd": cwd,
        "model": model,
        "provenance": {"kind": "fresh"},
        "version": "test",
        "created_at": 1000,
    });
    write(
        dir.join("session.json"),
        serde_json::to_string(&meta).expect("serialize descriptor"),
    )
    .expect("write descriptor");
}

/// Seed a session on disk WITH a checkpoint manifest — a post-compact state —
/// so a launched --resume <sid> followed by /context shows the folded
/// summary + the Compact buffer category. Writes the log (one UserInput
/// event), the session.json descriptor, and a CheckpointManifest via the real
/// LocalFileBackend (not hand-rolled JSON: the manifest schema can drift, and
/// the backend's read path would fail to deserialize a hand-written file).
/// The event is the manifest's last_event + the single Summarized turn, so
/// compact_summary returns a non-None line + the dispatch injects a
/// "Compact buffer" category. For the PTY /context post-compact journey.
///
/// write_checkpoint is a ContextBackend trait method returning a PFut
/// (internally synchronous, then wrapped); the helper is a sync fn called from
/// non-async PTY tests, so it blocks on the future directly rather than
/// spinning a tokio runtime.
pub fn seed_session_with_checkpoint(
    root: &Path,
    sid_str: &str,
    model: &str,
    prompt: &str,
    summary: &str,
) {
    // Reuse seed_session_with_cwd for the log + descriptor write + the event id,
    // so the two seed paths share one source of truth (a schema change to the
    // event or descriptor shape only has to be fixed once, not in two copies).
    let cwd = workspace_cwd(None);
    let event_id = seed_session_with_cwd(root, sid_str, model, prompt, &cwd);
    let sid = SessionId::from_display_string(sid_str).expect("sid parses");
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session: sid,
        last_event: event_id,
        summary: Some(summary.into()),
        plan: vec![TurnGroup {
            turn_id: event_id,
            disposition: Disposition::Summarized,
            event_ids: vec![event_id],
        }],
        ts: 0,
    };
    let backend = LocalFileBackend::new(root.to_path_buf());
    block_on(backend.write_checkpoint(manifest)).expect("write checkpoint");
}

/// One planted marker in a large fixture log: a unique whitespace-free token
/// and the byte depth of the event line that carries it. Depth lets a
/// measurement classify the marker as resident-zone (newer than the byte
/// budget from the tail) or disk-zone (only reachable through a history
/// read).
// Allowed dead because only the measurement binary calls the large-fixture
// seeder; the aggregated UI binary includes this module without using it.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct FixtureMarker {
    pub text: String,
    pub byte_depth: u64,
}

/// What a large fixture log holds, returned so a measurement can classify
/// samples without re-deriving the corpus.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct LargeFixture {
    pub sid: String,
    pub log_bytes: u64,
    pub event_count: usize,
    /// Markers in oldest-first order; the settle marker is not included.
    pub markers: Vec<FixtureMarker>,
    /// The unique token on the log's last event: the resume-replay settle
    /// latch (visible only after the whole trajectory reached the view).
    pub settle_marker: String,
}

/// The byte step between planted markers: a quarter of the history read's
/// window budget, so one loaded window reveals several markers and a
/// measurement can see each window land rather than only every fourth one.
#[allow(dead_code)]
const MARKER_STEP_BYTES: u64 = 64 * 1024;

/// Deterministic filler text: pseudo-words derived from the indices, so the
/// corpus is reproducible across runs without a random source. Each word is
/// six bytes ("w1234 ").
#[allow(dead_code)]
fn filler(turn: usize, phase: usize, words: usize) -> String {
    let mut text = String::with_capacity(words * 6 + 16);
    for i in 0..words {
        let v = (turn.wrapping_mul(31) + phase * 17 + i * 29) % 9973;
        text.push_str(&format!("w{v:04} "));
    }
    text
}

/// The buffered log writer for the large fixture, owning the file and the
/// running counters (byte depth, next timestamp, event count). The shapes
/// the generator writes — one event, the markers due at the current depth,
/// one turn — are its methods, so the counters stay with the file they
/// describe.
#[allow(dead_code)]
struct FixtureWriter {
    out: BufWriter<File>,
    sid: houyicoder_core::SessionId,
    depth: u64,
    ts_next: u64,
    event_count: usize,
}

#[allow(dead_code)]
impl FixtureWriter {
    fn new(path: &Path, sid: houyicoder_core::SessionId) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            out: BufWriter::with_capacity(1 << 20, file),
            sid,
            depth: 0,
            ts_next: 1_000,
            event_count: 0,
        })
    }

    /// Serialize one event as a JSONL line and track its byte depth.
    fn event(&mut self, event: houyicoder_core::SessionEvent) {
        let entry = SessionLogEntry {
            id: EventId::new(),
            session: self.sid,
            ts: self.ts_next,
            prev_hash: None,
            event,
        };
        let mut line = serde_json::to_string(&entry).expect("serialize event");
        line.push('\n');
        self.depth += line.len() as u64;
        self.ts_next += 1;
        self.event_count += 1;
        self.out.write_all(line.as_bytes()).expect("write log line");
    }

    /// Land a marker event whenever the depth crossed the next step: a
    /// standalone UserInput so the token renders on its own short line (no
    /// wrap-split, no merge into an adjacent message) and doubles as a turn
    /// boundary the lookback walk can stop on.
    fn due_markers(&mut self, markers: &mut Vec<FixtureMarker>, next_marker_at: &mut u64) {
        while self.depth >= *next_marker_at {
            let text = format!("MK{:04}QZX", markers.len());
            self.event(SessionEvent::UserInput { text: text.clone() });
            markers.push(FixtureMarker {
                text,
                byte_depth: self.depth,
            });
            *next_marker_at += MARKER_STEP_BYTES;
        }
    }

    /// One production-shaped turn: a start, a prompt, reasoning, a tool call
    /// and its result, and the answer.
    fn turn(&mut self, turn: usize) {
        self.event(SessionEvent::TurnStarted {
            turn: turn as u32,
            call_in_turn: 0,
        });
        self.event(SessionEvent::UserInput {
            text: format!("fixture prompt {turn}: inspect the recorded run"),
        });
        self.event(SessionEvent::Reasoning {
            text: filler(turn, 1, 300),
        });
        self.event(SessionEvent::ToolCall {
            call_id: format!("call-{turn}"),
            tool: "bash".into(),
            input: serde_json::json!({ "command": format!("grep -n 'fixture {turn}' run.log") }),
        });
        self.event(SessionEvent::ToolResult {
            call_id: format!("call-{turn}"),
            output: serde_json::json!({ "stdout": filler(turn, 2, 500) }),
            duration_ms: (turn % 40) as u64 + 5,
        });
        self.event(SessionEvent::AssistantMessage {
            text: filler(turn, 3, 200),
            thinking: None,
        });
    }

    /// Flush the buffer and hand back (depth, event count).
    fn finish(mut self) -> (u64, usize) {
        self.out.flush().expect("flush log");
        (self.depth, self.event_count)
    }
}

/// Seed a large session log (target_bytes magnitude) for measurement tests:
/// turns of TurnStarted + UserInput + Reasoning + ToolCall + ToolResult +
/// AssistantMessage with production-shaped payloads (the average event lands
/// near the 3.6 KB the history reader's window budget assumes), a unique
/// marker event every MARKER_STEP_BYTES of depth, and a settle marker on the
/// last event. The log is written directly as JSONL in one buffered pass —
/// the append-per-event path fsyncs each line, which a 64 MiB fixture
/// should not pay. The read path is lenient (prev_hash None matches the
/// existing seed precedent), and the descriptor mirrors seed_session_with_cwd
/// so --resume accepts the session. The event shape matches the CLI crate's
/// measurement fixture generator, so the index-build measurement and the
/// page-up measurement compare over the same turn layout; a change here
/// must be mirrored there.
#[allow(dead_code)]
pub fn seed_large_session_on_disk(
    root: &Path,
    sid_str: &str,
    model: &str,
    target_bytes: u64,
) -> LargeFixture {
    let sid = SessionId::from_display_string(sid_str).expect("sid parses");
    let dir = root.join(sid_str);
    create_dir_all(&dir).expect("mkdir session dir");

    let mut markers = Vec::new();
    let mut next_marker_at: u64 = MARKER_STEP_BYTES;
    let mut writer = FixtureWriter::new(&dir.join("log.jsonl"), sid).expect("create log");
    for turn in 0..usize::MAX {
        if writer.depth >= target_bytes {
            break;
        }
        writer.due_markers(&mut markers, &mut next_marker_at);
        writer.turn(turn);
    }
    let settle_marker = "SETTLEQZX".to_string();
    writer.event(SessionEvent::UserInput {
        text: settle_marker.clone(),
    });
    let (log_bytes, event_count) = writer.finish();
    // A write handle because syncing a file needs write access on Windows;
    // a read-only open fails there.
    OpenOptions::new()
        .write(true)
        .open(dir.join("log.jsonl"))
        .and_then(|f| f.sync_all())
        .expect("fsync log");

    let cwd = workspace_cwd(None);
    let meta = serde_json::json!({
        "name": null,
        "name_source": "auto",
        "cwd": cwd,
        "model": model,
        "provenance": {"kind": "fresh"},
        "version": "test",
        "created_at": 1000,
    });
    write(
        dir.join("session.json"),
        serde_json::to_string(&meta).expect("serialize meta"),
    )
    .expect("write descriptor");

    LargeFixture {
        sid: sid_str.to_string(),
        log_bytes,
        event_count,
        markers,
        settle_marker,
    }
}
