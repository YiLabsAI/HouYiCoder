//! Placeholder data builders for the TUI. The pure-stub app() returns an App
//! with no runner wired (legacy stub flow, used by Login/Console tests and the
//! fallback path). build_app_for_test() wires a real agent Runner so the working
//! screen does real chat: input spawns runner.run, the transcript is rebuilt
//! from real SessionLogEntries, and approvals come from real Interruptions.

pub(crate) mod terminal_guard;
pub mod terminal_title;

mod app_default;
mod placeholder;
mod switch;
mod worktree;
pub use app_default::app;
pub use placeholder::{context_view, failing_checks, help_text, release_notes};
pub use worktree::{WorktreeEntry, parse_worktrees};

use crate::agent_message::SessionMessage;
use crate::artifact::{ArtifactSession, StubProposer};
use crate::console_state::ConsoleState;
use crate::evidence::{
    AuditEntry, ConsoleTodo, DiffData, Divergence, GraphResult, Hunk, HunkEvidence, PlanArtifact,
    ReviewFinding, SpecArtifact, SpecClause, Verdict, VerifyResult, audit_entry,
};
use crate::palette::PaletteState;
use crate::records::{ContextSuggestion, SpecContext, SuggestionSeverity};
use crate::resume_picker::SessionCatalog;
use crate::review_queue::ReviewQueue;
use crate::session::Session;
use crate::session::SessionConnection;
use crate::session::identity::SessionIdentity;
use crate::session::persistence::SessionPersistence;
use crate::state::{App, Pane, Screen, Stage, ViewportMode};
use crate::transcript::snapshot::TranscriptSnapshot;
use crate::view::export_log::ExportLog;
use crate::view::trajectory_pane::TrajectoryLog;
#[cfg(test)]
use futures::channel::mpsc::channel;
use houyicoder_client::Client;
#[cfg(test)]
use houyicoder_client::InProcTransport;
#[cfg(test)]
use houyicoder_core::SessionId as EngineSessionId;
#[cfg(test)]
use houyicoder_core::agent::Runner;
#[cfg(test)]
use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
#[cfg(test)]
use houyicoder_permission::DefaultModeGate;
use houyicoder_protocol::frontend::LoginMode;
use houyicoder_protocol::frontend::SessionId;
use houyicoder_protocol::frontend::context::ContextBreakdown;
#[cfg(test)]
use houyicoder_service::CancellationToken;
#[cfg(test)]
use houyicoder_service::composition::fleet_status_relay;
#[cfg(test)]
use houyicoder_service::composition::{BuildRunnerOptions, build_runner};
#[cfg(test)]
use houyicoder_service::server::{EventSequencer, FrameCarrier, Server};
use std::env::current_dir;
#[cfg(test)]
use std::env::temp_dir;
use std::error::Error;
use std::path::PathBuf;
#[cfg(test)]
use std::process;
use std::sync::Arc;
use std::sync::OnceLock;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::mpsc;
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::runtime::{Builder, Runtime};
#[cfg(test)]
use tokio::sync::Notify;
#[cfg(test)]
use tokio::task::JoinHandle;

/// The wired protocol bundle the composition root (the CLI bin, or the test
/// pairing helper) builds and hands to the TUI: the protocol client the driver
/// task owns, the agent message channel the driver ships to, and the session
/// id. The runner + the permission gate stay with the server task — the TUI
/// holds no engine handle and no permission gate. The TUI never constructs
/// these itself; the composition root owns construction so the TUI stays a
/// presentation layer. The session values (client, channels, id, model) and
/// the host resources (trajectory, export, snapshot, catalog) travel here; the
/// frontend startup policy (skip-login, startup warnings, history path)
/// travels separately in FrontendStartup so the session commit does not mix
/// lifecycle domains.
pub struct RunnerBundle {
    /// The protocol client the driver task owns. Un-connected at hand-off;
    /// the driver performs the Hello handshake on spawn.
    pub client: Client,
    /// Sender cloned into the driver (permission asks + done + deltas routed
    /// from the wire). The TUI drains the receiver each poll tick.
    pub agent_tx: mpsc::Sender<SessionMessage>,
    /// Receiver drained by the TUI event loop each poll tick.
    pub agent_rx: mpsc::Receiver<SessionMessage>,
    /// The active session id (wire-typed). The engine session id is converted
    /// at the composition root, so the field and the production paths hold the
    /// wire type; only the test pairing helpers take the engine id.
    pub session: SessionId,
    /// The resolved model name, for the status bar display. The composition
    /// root resolves this; the TUI never calls the config layer itself.
    pub model: String,
    /// Injected trajectory source; None falls back to the mock trajectory.
    pub trajectory_log: Option<Arc<dyn TrajectoryLog>>,
    /// Injected export source; None makes /export report unavailable in this session.
    pub export_log: Option<Arc<dyn ExportLog>>,
    /// Injected session-log snapshot for the search view; None falls back to the in-memory vec.
    pub snapshot: Option<Arc<dyn TranscriptSnapshot>>,
    /// The session catalog for the /resume picker. None in stub/test.
    pub session_catalog: Option<Arc<dyn SessionCatalog>>,
}

/// Frontend startup policy the composition root resolves and the TUI applies
/// to its Ui and Input owners, separate from the session commit. skip_login
/// sets the landing screen and login mode; startup_warnings land as initial
/// transcript system lines before any run output; history_path wires the
/// prompt-history file the Input owner reads and appends to. These are not
/// session data — a Session never carries them — so the bundle that builds a
/// Session does not carry them either.
pub struct FrontendStartup {
    pub skip_login: bool,
    pub startup_warnings: Vec<String>,
    pub history_path: PathBuf,
}

/// A shared tokio runtime for tests. Building a multi-thread runtime with
/// enabled drivers per test is expensive (tens of ms each); hundreds of tests
/// would spend seconds constructing runtimes. The runtime is read-only after
/// construction and tasks are independent (separate channels/runners), so
/// sharing the thread pool is safe.
pub fn shared_runtime() -> Arc<Runtime> {
    static RT: OnceLock<Arc<Runtime>> = OnceLock::new();
    RT.get_or_init(|| {
        Arc::new(
            Builder::new_multi_thread()
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
    current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_default()
}

/// Wire a working-surface App to the wired bundle the composition root built.
/// Spawns the long-lived client driver, which owns the protocol client,
/// performs Hello, and routes ordered server frames onto the agent channel.
/// Lands on the Login screen like app(), so the login flow is unchanged. The
/// frontend startup policy (skip-login, warnings, history path) is applied
/// after the session wiring so the session commit does not mix lifecycle
/// domains.
pub fn build_app(bundle: RunnerBundle, startup: FrontendStartup) -> App {
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
        session_catalog,
    } = bundle;
    // The live session: spawns the driver task on the shared runtime and
    // hands App the command channel + the message receiver + the request-id
    // counter. The driver takes ownership of the client; the App holds only
    // the session handle. The driver accumulates the wire frame stream; it
    // holds no runner handle — run, resume, and streaming all cross the wire
    // to the server task that owns the runner, and the transcript rides the
    // accumulated frames — not a store replay.
    let driver_agent_tx = agent_tx.clone();
    let live_session =
        SessionConnection::spawn(client, driver_agent_tx, agent_rx, &shared_runtime());
    app.sessions.current.sandbox = "mac-seatbelt".to_string();
    // The launch model stands in for the applied model until the host
    // snapshot lands, so the status bar names what the session runs.
    app.sessions.current.capabilities.model.catalog.applied.id = model;
    app.sessions.current.identity.session_id = session_id;
    app.sessions.current.connection = Some(live_session);
    app.sessions.persistence = SessionPersistence {
        trajectory_log,
        export_log,
        snapshot,
    };
    app.sessions.catalog = session_catalog;
    app.apply_frontend_startup(startup);
    app
}

pub type ResumeBuilderRef = dyn Fn(&str) -> Result<(RunnerBundle, FrontendStartup), Box<dyn Error>>;

/// Build the runner bundle via the composition root and wire it, matching the
/// build_app_for_test(project) API so the existing test suite needs
/// no per-call change. Test-only: the pairing (server spawn + client
/// construction) uses the service dev-dep; the production composition root
/// lives in the CLI bin at runtime.
#[cfg(test)]
pub fn build_app_for_test(project: Option<String>) -> App {
    let mut options = BuildRunnerOptions::default();
    options.project = project;
    let bundle = build_runner(options);
    let wire_session = SessionId(bundle.session.to_string());
    let (tx, rx) = mpsc::channel::<SessionMessage>();
    let (runner, client, startup_warnings) = start_local_server(
        bundle.runner,
        bundle.session,
        bundle.gate,
        bundle.append_notify,
        bundle.bus,
    );
    drop(runner); // server owns the runner; the TUI holds no engine handle.
    build_app(
        RunnerBundle {
            client,
            agent_tx: tx,
            agent_rx: rx,
            session: wire_session,
            model: "test-model".to_string(),
            trajectory_log: None,
            export_log: None,
            snapshot: None,
            session_catalog: None,
        },
        FrontendStartup {
            skip_login: false,
            startup_warnings,
            history_path: temp_dir().join(format!(
                "houyi-history-test-{}-{}.jsonl",
                process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            )),
        },
    )
}

/// Pair an in-memory server and client around a runner. The test-only helper
/// installs one event sequencer before sharing the runner and gives the same
/// sequencer to the server, matching the CLI composition root.
#[cfg(test)]
pub fn start_local_server(
    runner: Runner,
    session: EngineSessionId,
    gate: Arc<DefaultModeGate>,
    append_notify: Arc<Notify>,
    bus: Option<Arc<AgentBus>>,
) -> (Arc<Runner>, Client, Vec<String>) {
    let (runner, client, _serve, warnings) =
        start_local_server_tracked(runner, session, gate, append_notify, bus);
    (runner, client, warnings)
}

/// Same as start_local_server but returns the server task's JoinHandle so a
/// test can assert the serve loop exited (e.g. after a swap drops the old
/// session: cmd_tx drop -> driver exits -> client drop -> c2s_tx drop ->
/// server next_frame None -> serve returns). The handle is detached in the
/// non-tracked variant; tests that need to verify teardown use this one.
#[cfg(test)]
pub fn start_local_server_tracked(
    mut runner: Runner,
    session: EngineSessionId,
    gate: Arc<DefaultModeGate>,
    append_notify: Arc<Notify>,
    bus: Option<Arc<AgentBus>>,
) -> (Arc<Runner>, Client, JoinHandle<()>, Vec<String>) {
    let (c2s_tx, c2s_rx) = channel(16);
    let (s2c_tx, s2c_rx) = channel(16);
    let event_sequencer = EventSequencer::new();
    event_sequencer.install_on(&mut runner);
    // Fleet status relay: translate bus child status into AgentStatus wire
    // frames so the TUI footer renders without touching the engine bus.
    fleet_status_relay::spawn(
        bus,
        event_sequencer.clone(),
        shared_runtime().handle().clone(),
        CancellationToken::new(),
    );
    // Drain startup warnings synchronously before the runner is shared so the
    // host can push them as initial transcript system lines — no async-sink
    // race with later command output or test assertions.
    let startup_warnings = runner.drain_startup_warnings();
    let runner = Arc::new(runner);
    let server_io = FrameCarrier::new(s2c_tx, c2s_rx);
    let gate_dyn: Arc<dyn houyicoder_permission::ModeGate> = gate;
    let server =
        Server::new_with_event_sequencer(runner.clone(), session, gate_dyn, event_sequencer);
    // Test isolation: the server's settings path defaults to the real config
    // file, so a model pick persisted the test's id into the developer's real
    // settings on every check run. Redirect to a unique temp path under
    // cfg(test); the production binary keeps the real path.
    #[cfg(test)]
    let server = server.with_settings_path(local_test_settings_path());
    let server = server.with_append_notify(append_notify);
    let runtime = shared_runtime();
    let serve_handle = runtime.spawn(async move {
        let _serve = server.serve(server_io).await;
    });
    let transport = InProcTransport::from_halves(c2s_tx, s2c_rx);
    let client = Client::new(Box::new(transport));
    (runner, client, serve_handle, startup_warnings)
}

/// A unique temp settings path for one in-process test server, so a /model
/// persist writes the temp file instead of the developer's real config.
#[cfg(test)]
fn local_test_settings_path() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    temp_dir().join(format!("houyi-settings-{}-{}.json", process::id(), n))
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
pub fn suggestions_for(breakdown: &ContextBreakdown) -> Vec<ContextSuggestion> {
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
mod tests;
