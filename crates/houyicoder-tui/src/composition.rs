//! Placeholder data builders for the TUI. The pure-stub app() returns an App
//! with no runner wired (legacy stub flow, used by Login/Console tests and the
//! fallback path). build_app_for_test() wires a real agent Runner so the working
//! screen does real chat: input spawns runner.run, the transcript is rebuilt
//! from real TurnEvents, and approvals come from real Interruptions.

mod app_default;
mod placeholder;
mod worktree;
pub use app_default::app;
pub use placeholder::*;
pub use worktree::{WorktreeEntry, parse_worktrees};

use crate::artifact::{ArtifactSession, StubProposer};
use crate::console_state::ConsoleState;
use crate::evidence::{
    AgentStatus, AuditEntry, ConsoleTodo, DiffData, Divergence, GraphResult, Hunk, HunkEvidence,
    MemoryEntry, PlanArtifact, ReviewFinding, SpecArtifact, SpecClause, Verdict, VerifyResult,
    audit_entry,
};
use crate::palette::PaletteState;
use crate::review_queue::ReviewQueue;
use crate::scroll::{SearchState, TranscriptScroll};
use crate::selection::Selection;
use crate::state::{
    App, Pane, PermissionInput, PermissionTab, Screen, SpecContext, Stage, StatusStub,
    TranscriptLine, ViewportMode,
};
use houyicoder_client::Client;
#[cfg(test)]
use houyicoder_core::SessionId;
#[cfg(test)]
use houyicoder_core::agent::Runner;
#[cfg(test)]
use houyicoder_permission::DefaultModeGate;
use ratatui::layout::Rect;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
#[cfg(test)]
use std::sync::Arc;
use std::sync::mpsc;

/// The wired protocol bundle the composition root (the CLI bin, or the test
/// pairing helper) builds and hands to the TUI: the protocol client the driver
/// task owns, the agent message channel the driver ships to, and the session
/// id. The runner + the permission gate stay with the server task — the TUI
/// holds no engine handle and no permission gate. The TUI never constructs
/// these itself; the composition root owns construction so the TUI stays a
/// presentation layer.
pub struct RunnerBundle {
    /// The protocol client the driver task owns. Un-connected at hand-off;
    /// the driver performs the Hello handshake on spawn.
    pub client: Client,
    /// Sender cloned into the driver (permission asks + done + deltas routed
    /// from the wire). The TUI drains the receiver each poll tick.
    pub agent_tx: mpsc::Sender<crate::run_control::AgentMessage>,
    /// Receiver drained by the TUI event loop each poll tick.
    pub agent_rx: mpsc::Receiver<crate::run_control::AgentMessage>,
    /// The active session id (wire-typed). The engine session id is converted
    /// at the composition root; the TUI never imports the engine SessionId.
    pub session: houyicoder_protocol::frontend::SessionId,
    /// The resolved model name, for the status bar display. The composition
    /// root resolves this; the TUI never calls the config layer itself.
    pub model: String,
    /// Injected TrajectoryLog bridge; None falls back to the mock trajectory.
    pub trajectory_log: Option<std::sync::Arc<dyn crate::view::trajectory_pane::TrajectoryLog>>,
    /// Injected ExportLog bridge; None makes /export report no session log wired.
    pub export_log: Option<std::sync::Arc<dyn crate::view::export_log::ExportLog>>,
    /// Injected snapshot bridge (loads the durable log for the search view); None falls back to the in-memory vec.
    pub snapshot: Option<std::sync::Arc<dyn crate::transcript::snapshot::TranscriptSnapshot>>,
    /// The session-listing bridge for the /resume picker. None in stub/test.
    pub session_lister: Option<std::sync::Arc<dyn crate::resume_picker::SessionLister>>,
    pub skip_login: bool,
    /// Startup warnings (bad settings fields, network-policy typos) the host
    /// pushes as initial transcript system lines. Drained from the runner at
    /// pair time so they land before any run output (no async-sink race).
    /// Empty on the stub/test path.
    pub startup_warnings: Vec<String>,
}

/// A shared tokio runtime for tests. Building a multi-thread runtime with
/// enabled drivers per test is expensive (tens of ms each); hundreds of tests
/// would spend seconds constructing runtimes. The runtime is read-only after
/// construction and tasks are independent (separate channels/runners), so
/// sharing the thread pool is safe.
pub fn shared_runtime() -> std::sync::Arc<tokio::runtime::Runtime> {
    use std::sync::OnceLock;
    static RT: OnceLock<std::sync::Arc<tokio::runtime::Runtime>> = OnceLock::new();
    RT.get_or_init(|| {
        std::sync::Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .expect("tokio runtime"),
        )
    })
    .clone()
}

/// The session's original working directory as a display string, shown at the
/// top of the /permissions Workspace tab. Empty when the cwd cannot be read
/// (a sandboxed or stub context).
fn cwd_string() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_default()
}

/// Wire a working-surface App to the wired bundle the composition root built.
/// Spawns the long-lived client-driver task (it owns the protocol client,
/// performs the Hello handshake, and drains server frames, shipping streamed
/// deltas forwarded from the shared runner's live sink, mid-turn permission
/// asks, and the final outcome as AgentMessage on the agent channel). The
/// driver + the server task share the one Arc<Runner> the composition root
/// built, so the live sink fires during the server's run without wire
/// streaming. Lands on the Login screen like app(), so the login flow is
/// unchanged.
pub fn build_app(bundle: RunnerBundle) -> App {
    let mut app = app();
    let RunnerBundle {
        client,
        agent_tx,
        agent_rx,
        session: session_id,
        model,
        trajectory_log,
        export_log,
        snapshot,
        session_lister,
        skip_login,
        startup_warnings,
    } = bundle;
    let runtime = shared_runtime();
    // The live session: spawns the driver task on the shared runtime and
    // hands App the command channel + the message receiver + the request-id
    // counter. The driver takes ownership of the client; the App holds only
    // the session handle. The driver accumulates the wire frame stream; it
    // holds no runner handle — run, resume, and streaming all cross the wire
    // to the server task that owns the runner, and the transcript rides the
    // accumulated frames — not a store replay.
    let driver_agent_tx = agent_tx.clone();
    let live_session = crate::session::Session::spawn(client, driver_agent_tx, agent_rx, &runtime);
    if skip_login {
        app.screen = crate::state::Screen::Working;
        app.login_mode = Some(houyicoder_protocol::frontend::LoginMode::Local);
    }
    app.status.model = model;
    app.status.sandbox = "mac-seatbelt".to_string();
    app.session_id = session_id;
    app.runtime = Some(runtime);
    app.agent_tx = Some(agent_tx);
    app.session = Some(live_session);
    app.trajectory_log = trajectory_log;
    app.export_log = export_log;
    app.snapshot = snapshot;
    app.session_lister = session_lister;
    // Push the drained startup warnings as initial system lines BEFORE any run
    // output — synchronous so they land first + do not race with command
    // results (the async-sink path flaked timing tests; this is deterministic).
    for w in startup_warnings {
        app.system_line(w);
    }
    app
}

pub type ResumeBuilderRef = dyn Fn(&str) -> Result<RunnerBundle, Box<dyn std::error::Error>>;

impl App {
    pub fn try_swap_session(
        &mut self,
        resume_builder: Option<&ResumeBuilderRef>,
        dirty: &mut bool,
    ) {
        let Some(target) = self.pending_resume_target.take() else {
            return;
        };
        // The caller (the event loop's idle guard) already gates on
        // !agent_busy && !reverse_request_in_flight, so a target that
        // survives to here is ready to swap. The old busy put-back branch
        // is gone -- it was a workaround for try_swap_session being called
        // every frame (even mid-run); polling a continuous state (agent_busy)
        // + putting the target back each frame forced the "no system_line or
        // it floods" hack. The drain now binds to the consume action, not the
        // idle condition.
        if let Some(builder) = resume_builder {
            match builder(&target) {
                Ok(new_bundle) => {
                    self.swap_session(new_bundle);
                    *dirty = true;
                }
                Err(e) => {
                    self.system_line(format!("resume failed: {e}"));
                    *dirty = true;
                }
            }
        } else {
            self.pending_resume_target = Some(target);
            self.quit = true;
        }
    }

    /// The event loop's idle drain: continuous-state polling + consumptive
    /// idempotency. !agent_busy holds every frame; no flood because drain/take
    /// CONSUMES the item, so the next frame has nothing to do. Side effects
    /// here MUST bind to the consume action, not the idle condition (binding
    /// to the condition floods -- that was the old try_swap_session busy
    /// branch's flaw). A queued item auto-sends ONLY when the prior run ended
    /// FinalOutput (a clean end) -- the user got their answer, so drain FIFO.
    /// An interrupt/error does NOT auto-send: the queued item stays parked for
    /// the user to recall to the input box + edit + re-send. A redirect on
    /// interrupt should not auto-fire the pending input — the user pops it
    /// via Esc (busy-Esc abort+pop, or idle-Esc pop) + edits before re-sending.
    pub fn idle_drain(&mut self, resume_builder: Option<&ResumeBuilderRef>, dirty: &mut bool) {
        if !self.agent_busy && !self.reverse_request_in_flight() {
            self.try_swap_session(resume_builder, dirty);
            if self.status.last_run_final && self.drain_pending_head() {
                *dirty = true;
            }
        }
    }

    pub fn swap_session(&mut self, bundle: RunnerBundle) {
        // Reverse-default: a swap is "fresh App + new bundle" -- rebuild from
        // build_app (the launch-time equivalent), which resets every
        // session-local field via app()'s defaults and applies only the bundle.
        // This terminates the whitelist-clear approach that kept leaking fields
        // (todos, bash_progress, selection, run-domain req ids, ...) across
        // swaps: a new session-local field added to App cannot leak, because
        // nothing is carried over from the old self except what build_app
        // explicitly sets. Bump transcript_version to invalidate the slots
        // cache (build_app leaves it at the default 0; the bump is explicit
        // so a stale cached render does not ride into the new session).
        //
        // The pending queue is carried across (a session switch does not
        // clear the pending queue — only session-scoped fields). Carried
        // items auto-drain in the new session on the next idle: a queued
        // message from session A sends in session B as the next turn. The
        // one deliberate divergence: session-scoped Commands (/clear /rewind
        // /undo) the user typed in the OLD session are DROPPED, not carried
        // -- they operate on the current session, so carrying + auto-draining
        // them would apply the old session's intent to the new one (a /clear
        // typed in A would clear B). /resume Commands stay (switch intent is
        // still valid); Messages stay (user content). Dropping old-session
        // Commands prevents a stale /clear firing in B.
        let mut pending = std::mem::take(&mut self.pending);
        // Drop session-scoped Commands (/clear /rewind /undo) the user typed
        // in the OLD session: they operate on the current session, so
        // carrying them to the NEW session + auto-draining would apply the
        // old session's intent to the new one (a /clear typed in A would
        // clear B). /resume Commands stay -- they express a switch intent
        // that is still valid in the new session. Messages stay -- they are
        // user content, recallable + auto-drainable in the new session.
        let mut dropped: Vec<String> = Vec::new();
        pending.retain(|item| {
            if let crate::pending_queue::PendingItem::Command(text) = item
                && !crate::pending_queue::command_first_token_is(text, "resume")
            {
                dropped.push(text.clone());
                return false;
            }
            true
        });
        // The new runner's server queue is empty, so a carried Message
        // (InjectUser'd to the old server) lost its server copy. Demote to
        // ParkedMessage so the barrier holds + the drain spawns it fresh.
        for item in pending.iter_mut() {
            if let crate::pending_queue::PendingItem::Message(t) = item {
                *item = crate::pending_queue::PendingItem::ParkedMessage(t.clone());
            }
        }
        *self = build_app(bundle);
        // A swap is always initiated from the working screen (the user is on
        // the transcript when they /resume), so force Working regardless of the
        // bundle's skip_login. This nails the skip_login coupling: a bundle
        // that carried skip_login=false would otherwise drop the user back to
        // the login screen mid-session. Every current swap bundle passes
        // skip_login=true, but this one line makes that invariant unbreakable.
        self.screen = crate::state::Screen::Working;
        let v = self.transcript_version.get().wrapping_add(1);
        self.transcript_version.set(v);
        self.pending = pending;
        // A swap is a clean transition (the prior run ended FinalOutput, the
        // /resume Command drained at idle, then the swap ran). Carried items
        // auto-drain in the new session (a queued message from A sends in B
        // as the next turn). build_app reset last_run_final to false, so
        // re-set it true here or the carried queue would park (re-introducing
        // the old carry-park behavior).
        self.status.last_run_final = true;
        if !dropped.is_empty() {
            self.system_line(format!(
                "resume: dropped {} command(s) from the old session ({}) -- re-issue in the new session if needed",
                dropped.len(),
                dropped.join(", ")
            ));
        }
    }
}

/// Build the runner bundle via the composition root and wire it, matching the
/// build_app_for_test(project) API so the existing test suite needs
/// no per-call change. Test-only: the pairing (server spawn + client
/// construction) uses the service dev-dep; the production composition root
/// lives in the CLI bin at runtime.
#[cfg(test)]
pub fn build_app_for_test(project: Option<String>) -> App {
    let mut options = houyicoder_service::composition::BuildRunnerOptions::default();
    options.project = project;
    let bundle = houyicoder_service::composition::build_runner(options);
    let wire_session = houyicoder_protocol::frontend::SessionId(bundle.session.to_string());
    let (tx, rx) = mpsc::channel::<crate::run_control::AgentMessage>();
    let (runner, client, startup_warnings) = pair_inproc_server(
        bundle.runner,
        bundle.session,
        bundle.gate,
        bundle.append_notify,
    );
    drop(runner); // server owns the runner; the TUI holds no engine handle.
    build_app(RunnerBundle {
        client,
        agent_tx: tx,
        agent_rx: rx,
        session: wire_session,
        model: "test-model".to_string(),
        trajectory_log: None,
        export_log: None,
        snapshot: None,
        session_lister: None,
        skip_login: false,
        startup_warnings,
    })
}

/// Pair an in-memory server + client around a runner, install the live delta
/// sink, spawn the server on the shared runtime, and return the shared runner
/// handle plus the un-connected client. Test-only composition helper: the
/// production equivalent lives in the CLI bin (the layering-compliant
/// composition root; service cannot be a runtime dep of the TUI). The delta
/// sink streams acpx/llm/* notifications onto the wire during the server's
/// run (installed before Arc so the server task and any holder share one
/// runner); the shared event-seq counter keeps live deltas and durable turn
/// events on one monotonic seq stream.
#[cfg(test)]
pub fn pair_inproc_server(
    runner: Runner,
    session: SessionId,
    gate: Arc<DefaultModeGate>,
    append_notify: Arc<tokio::sync::Notify>,
) -> (Arc<Runner>, Client, Vec<String>) {
    let (runner, client, _serve, warnings) =
        pair_inproc_server_tracked(runner, session, gate, append_notify);
    (runner, client, warnings)
}

/// Same as pair_inproc_server but returns the server task's JoinHandle so a
/// test can assert the serve loop exited (e.g. after a swap drops the old
/// session: cmd_tx drop -> driver exits -> client drop -> c2s_tx drop ->
/// server next_frame None -> serve returns). The handle is detached in the
/// non-tracked variant; tests that need to verify teardown use this one.
#[cfg(test)]
pub fn pair_inproc_server_tracked(
    mut runner: Runner,
    session: SessionId,
    gate: Arc<DefaultModeGate>,
    append_notify: Arc<tokio::sync::Notify>,
) -> (
    Arc<Runner>,
    Client,
    tokio::task::JoinHandle<()>,
    Vec<String>,
) {
    let (c2s_tx, c2s_rx) = futures::channel::mpsc::channel(16);
    let (s2c_tx, s2c_rx) = futures::channel::mpsc::channel(16);
    let next_seq = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    houyicoder_service::server::install_delta_sink(&mut runner, s2c_tx.clone(), next_seq.clone());
    // Drain startup warnings synchronously before the runner is shared so the
    // host can push them as initial transcript system lines — no async-sink
    // race with later command output or test assertions.
    let startup_warnings = runner.drain_startup_warnings();
    let runner = Arc::new(runner);
    let server_io = houyicoder_service::server::ServerIo::new(s2c_tx, c2s_rx);
    let gate_dyn: Arc<dyn houyicoder_permission::ModeGate> = gate;
    let server = houyicoder_service::server::Server::new_with_shared_seq(
        runner.clone(),
        session,
        gate_dyn,
        next_seq,
    )
    .with_append_notify(append_notify);
    let runtime = shared_runtime();
    let serve_handle = runtime.spawn(async move {
        let _serve = server.serve(server_io).await;
    });
    let transport = houyicoder_client::InProcTransport::from_halves(c2s_tx, s2c_rx);
    let client = Client::new(Box::new(transport));
    (runner, client, serve_handle, startup_warnings)
}

fn transcript() -> Vec<TranscriptLine> {
    // The working surface starts empty — the input box carries a dim
    // placeholder hint (describe a change or / for commands) that vanishes
    // on the first keystroke, so the top stays clean instead of a welcome
    // line floating far from the input.
    Vec::new()
}

fn status() -> StatusStub {
    StatusStub {
        session_id: "sess-stub".to_string(),
        model: "model-stub".to_string(),
        path: "~/workspace/hicoder".to_string(),
        tokens: 0,
        capability: "deny-by-default".to_string(),
        sandbox: "off".to_string(),
        plan_mode: false,
        last_run_final: false,
    }
}

fn spec_context() -> SpecContext {
    SpecContext {
        spec_id: "spec-001".to_string(),
        title: "fix the stub bug".to_string(),
        step: "idle".to_string(),
        clause_focus: 1,
    }
}

fn spec_clauses() -> Vec<SpecClause> {
    // All clauses start unimplemented: no hunk is approved yet. As hunks are
    // approved in the implement stage, each clause moves to partial; at verify
    // time, clauses with approved hunks move to satisfied. The strip reflects
    // this live (unimpl -> partial -> satisfied).
    vec![
        SpecClause {
            id: "clause-1".to_string(),
            text: "the placeholder bug no longer reproduces".to_string(),
            status: Divergence::Unimplemented,
        },
        SpecClause {
            id: "clause-2".to_string(),
            text: "fn main returns Result".to_string(),
            status: Divergence::Unimplemented,
        },
        SpecClause {
            id: "clause-3".to_string(),
            text: "no new warnings under clippy -D warnings".to_string(),
            status: Divergence::Unimplemented,
        },
    ]
}

fn diff_data() -> DiffData {
    DiffData {
        path: "src/lib.rs".to_string(),
        focus: 0,
        hunks: vec![
            Hunk {
                id: "change-1".to_string(),
                file: "src/lib.rs".to_string(),
                range: "1-4".to_string(),
                patch: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,4 @@\n fn main() {\n+    // placeholder edit\n }\n".to_string(),
                evidence: HunkEvidence {
                    spec_clause_id: "clause-1".to_string(),
                    spec_clause_desc: "the placeholder bug no longer reproduces".to_string(),
                    finding_id: "S-1".to_string(),
                    finding_desc: "correctness: change is minimal and sound".to_string(),
                    test_id: "test_bug_not_reproduced".to_string(),
                    why: "implements clause-1, covered by test_bug_not_reproduced".to_string(),
                },
                approved: Verdict::Pending,
            },
            Hunk {
                id: "change-2".to_string(),
                file: "src/lib.rs".to_string(),
                range: "12-18".to_string(),
                patch: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -12,6 +12,7 @@\n-    return Ok(())\n+    Ok(())\n".to_string(),
                evidence: HunkEvidence {
                    spec_clause_id: "clause-2".to_string(),
                    spec_clause_desc: "fn main returns Result".to_string(),
                    finding_id: "S-2".to_string(),
                    finding_desc: "security: exit code lost when main returns unit".to_string(),
                    test_id: "test_main_returns_result".to_string(),
                    why: "implements clause-2, covered by test_main_returns_result".to_string(),
                },
                approved: Verdict::Pending,
            },
            Hunk {
                id: "change-3".to_string(),
                file: "src/lib.rs".to_string(),
                range: "20-24".to_string(),
                patch: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -20,3 +20,5 @@\n+    #[cfg(test)]\n+    mod tests {}\n".to_string(),
                evidence: HunkEvidence {
                    spec_clause_id: "clause-3".to_string(),
                    spec_clause_desc: "no new warnings under clippy -D warnings".to_string(),
                    finding_id: "S-3".to_string(),
                    finding_desc: "style: missing test module scaffold".to_string(),
                    test_id: "missing".to_string(),
                    why: "advances clause-3; test coverage still missing".to_string(),
                },
                approved: Verdict::Pending,
            },
        ],
    }
}

fn spec_artifact() -> SpecArtifact {
    SpecArtifact {
        id: "spec-001".to_string(),
        title: "placeholder spec: fix the stub bug".to_string(),
        acceptance: vec![
            "the placeholder bug no longer reproduces".to_string(),
            "fn main returns Result".to_string(),
            "no new warnings under clippy -D warnings".to_string(),
        ],
        contract: vec!["FsWrite edits stay within the crate".to_string()],
        test_plan: vec![
            "test_bug_not_reproduced".to_string(),
            "test_main_returns_result".to_string(),
        ],
        approved: false,
    }
}

fn plan_artifact() -> PlanArtifact {
    PlanArtifact {
        id: "plan-001".to_string(),
        steps: vec![
            "read src/lib.rs".to_string(),
            "graph impact_set for the edit site".to_string(),
            "apply the placeholder edit".to_string(),
            "run tests and clippy".to_string(),
        ],
        approved: false,
    }
}

fn review_findings() -> Vec<ReviewFinding> {
    vec![
        ReviewFinding {
            id: "S-1".to_string(),
            lens: "correctness".to_string(),
            verdict: "refuted".to_string(),
            severity: "info".to_string(),
            hunk_id: "change-1".to_string(),
            spec_clause_id: "clause-1".to_string(),
            test_id: "test_bug_not_reproduced".to_string(),
            adversarial: "3 lens: 2 refuted / 1 real -> weighted refuted".to_string(),
            note: "change is minimal and correct (placeholder)".to_string(),
            signoff: Verdict::Pending,
        },
        ReviewFinding {
            id: "S-2".to_string(),
            lens: "security".to_string(),
            verdict: "real".to_string(),
            severity: "high".to_string(),
            hunk_id: "change-2".to_string(),
            spec_clause_id: "clause-2".to_string(),
            test_id: "test_main_returns_result".to_string(),
            adversarial: "3 lens: 2 real / 1 refuted -> weighted real".to_string(),
            note: "exit code lost when main returns unit (placeholder)".to_string(),
            signoff: Verdict::Pending,
        },
        ReviewFinding {
            id: "S-3".to_string(),
            lens: "style".to_string(),
            verdict: "real".to_string(),
            severity: "medium".to_string(),
            hunk_id: "change-3".to_string(),
            spec_clause_id: "clause-3".to_string(),
            test_id: "missing".to_string(),
            adversarial: "3 lens: 3 real -> weighted real".to_string(),
            note: "test coverage missing for clause-3 (placeholder)".to_string(),
            signoff: Verdict::Pending,
        },
    ]
}

fn audit_trail() -> Vec<AuditEntry> {
    vec![
        audit_entry("S-0", "signed off", "reviewer@hicoder", "2026-06-25T09:12"),
        audit_entry("S-1", "rejected", "reviewer@hicoder", "2026-06-25T09:14"),
    ]
}

fn verify_result() -> VerifyResult {
    VerifyResult {
        checks: vec![
            "cargo test --workspace: PASS (placeholder)".to_string(),
            "clippy -D warnings: clean (placeholder)".to_string(),
            "fmt --check: clean (placeholder)".to_string(),
        ],
        passed: true,
    }
}

fn graph_result() -> GraphResult {
    GraphResult {
        query: "impact_set(src/lib.rs)".to_string(),
        impact: vec![
            "src/lib.rs".to_string(),
            "src/main.rs".to_string(),
            "tests/lib_test.rs".to_string(),
        ],
    }
}

fn memory_entries() -> Vec<MemoryEntry> {
    vec![
        MemoryEntry {
            topic: "build-gate".to_string(),
            summary: "make check must stay green before commit (placeholder)".to_string(),
            scope: "project".to_string(),
            source: "project".to_string(),
        },
        MemoryEntry {
            topic: "comment-style".to_string(),
            summary: "no CJK, no backtick identifiers in .rs comments (placeholder)".to_string(),
            scope: "user".to_string(),
            source: "feedback".to_string(),
        },
        MemoryEntry {
            topic: "spec-driven".to_string(),
            summary: "every change cites a spec clause and a test id (placeholder)".to_string(),
            scope: "auto".to_string(),
            source: "reference".to_string(),
        },
    ]
}

fn agents() -> Vec<AgentStatus> {
    // Empty in v0: no live child fleet exists until child-tracking lands.
    // The /agents pane shows the fetched agent directory instead; the fleet
    // list renders only when child events populate this field.
    Vec::new()
}

fn console_todos() -> Vec<ConsoleTodo> {
    vec![
        ConsoleTodo {
            kind: "PR".to_string(),
            title: "#142 fix login redirect (placeholder)".to_string(),
            state: "review".to_string(),
        },
        ConsoleTodo {
            kind: "issue".to_string(),
            title: "#200 flaky CI on mac (placeholder)".to_string(),
            state: "assigned".to_string(),
        },
    ]
}

/// Build the canned /context view payload for the no-runner path: the
/// proportional grid + categories (from the core stub breakdown), plus
/// drill-down rows (memory files, skills) and canned suggestions derived
/// from the breakdown so the inline block renders the real layout
/// end-to-end before the analyzer is wired.
/// Context-usage suggestions derived from a breakdown fill ratio and per-category
/// size: the autocompact-near, heavy-bash-results, and heavy-read-results
/// hints. Shared by the stub and the real /context path so the logic is not
/// duplicated.
pub fn suggestions_for(
    breakdown: &houyicoder_protocol::frontend::context::ContextBreakdown,
) -> Vec<crate::records::ContextSuggestion> {
    use crate::records::{ContextSuggestion, SuggestionSeverity};
    let pct = if breakdown.context_window > 0 {
        100.0 * breakdown.total_tokens as f64 / breakdown.context_window as f64
    } else {
        0.0
    };
    let mut out: Vec<ContextSuggestion> = Vec::new();
    if pct >= 80.0 {
        out.push(ContextSuggestion {
            severity: SuggestionSeverity::Warning,
            title: format!("Context is {:.0}% full", pct),
            detail: "Autocompact will trigger soon, which discards older messages. \
                     Use /compact now to control what gets kept."
                .to_string(),
            savings_tokens: None,
        });
    }
    for cat in &breakdown.categories {
        if cat.label == "Messages" && cat.tokens > 10_000 && breakdown.context_window > 0 {
            let c_pct = 100.0 * cat.tokens as f64 / breakdown.context_window as f64;
            if c_pct >= 15.0 {
                out.push(ContextSuggestion {
                    severity: SuggestionSeverity::Warning,
                    title: format!("Conversation using {} tokens ({:.0}%)", cat.tokens, c_pct),
                    detail: "Run /compact to fold older turns into a summary.".to_string(),
                    savings_tokens: Some((cat.tokens as f64 * 0.5) as u32),
                });
            }
        }
        if cat.label == "System tools" && cat.tokens > 5_000 && breakdown.context_window > 0 {
            let c_pct = 100.0 * cat.tokens as f64 / breakdown.context_window as f64;
            if c_pct >= 5.0 {
                out.push(ContextSuggestion {
                    severity: SuggestionSeverity::Info,
                    title: format!("Tool schemas using {} tokens ({:.0}%)", cat.tokens, c_pct),
                    detail: "Disable unused tools to shrink the tool-docs prefix.".to_string(),
                    savings_tokens: Some((cat.tokens as f64 * 0.3) as u32),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod swap_tests;
