//! Placeholder data builders for the TUI. The pure-stub app() returns an App
//! with no runner attached (legacy stub flow, used by Login/Console tests and the
//! fallback path). build_app() wires the handoff the composition root built, so
//! the working screen does real chat: input spawns a run, the transcript is
//! rebuilt from real session entries, and approvals come from real
//! interruptions.

pub(crate) mod terminal_guard;
pub(crate) mod terminal_title;
pub(crate) mod termination;

mod app_default;
mod placeholder;
pub mod suggestions;
mod switch;
mod worktree;
pub use app_default::app;
pub use placeholder::{context_view, failing_checks, help_text, release_notes};
pub use suggestions::suggestions_for;
pub use worktree::{WorktreeEntry, parse_worktrees};

use crate::agent_message::SessionMessage;
use crate::artifact::{ArtifactSession, StubProposer};
use crate::console_state::ConsoleState;
use crate::evidence::{
    AuditEntry, ConsoleTodo, DiffData, Divergence, GraphResult, Hunk, HunkEvidence, PlanArtifact,
    ReviewFinding, SpecArtifact, SpecClause, Verdict, VerifyResult, audit_entry,
};
use crate::palette::PaletteState;
use crate::records::SpecContext;
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
use houyicoder_client::Client;
use houyicoder_protocol::frontend::LoginMode;
use houyicoder_protocol::frontend::SessionId;
use std::env::current_dir;
use std::env::var;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc;
use tokio::runtime::{Builder, Runtime};

/// The session handoff the composition root (the CLI bin, or a test
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
pub struct SessionHandoff {
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

/// Attach a working-surface App to the session handoff the composition root built.
/// Spawns the long-lived client driver, which owns the protocol client,
/// performs Hello, and routes ordered server frames onto the agent channel.
/// Lands on the Login screen like app(), so the login flow is unchanged. The
/// frontend startup policy (skip-login, warnings, history path) is applied
/// after the session wiring so the session commit does not mix lifecycle
/// domains.
pub fn build_app(bundle: SessionHandoff, startup: FrontendStartup) -> App {
    let mut app = app();
    let SessionHandoff {
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
    let driver_agent_tx = agent_tx.clone();
    let live_session =
        SessionConnection::spawn(client, driver_agent_tx, agent_rx, &shared_runtime());
    app.sessions.current.sandbox = if var("HOUYICODER_SANDBOX_NO_ENFORCE").is_ok_and(|v| v == "1") {
        "off (no-sandbox)".to_string()
    } else {
        "mac-seatbelt".to_string()
    };
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

pub type ResumeBuilderRef =
    dyn Fn(&str) -> Result<(SessionHandoff, FrontendStartup), Box<dyn Error>>;

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

#[cfg(test)]
mod tests;
