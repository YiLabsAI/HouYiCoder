//! The re-derivable compaction backbone: a structured block derived from the
//! session event log + the workspace at compact time, placed after the LLM
//! summary with the backbone authoritative on conflict (derived > recalled).
//! The LLM summarizer path runs unchanged (v1 is add-only: the backbone is
//! added, the LLM is not shrunk).
//!
//! Three-layer derivation (log > watermarked workspace > non-rederivable):
//! (a) Log-rederivable (preferred, never drifts): the file-touch set is read
//!     off edit/read/write tool calls; test results off bash tool results; the
//!     todo snapshot off the last todo_write call. The append-only log never
//!     mutates, so re-deriving from the same log yields the same backbone
//!     (the log_rederivable_no_staleness invariant).
//! (b) Workspace-rederivable (next-best, watermarked): the git revision + a
//!     hash of the dirty-tree porcelain come from the live workspace, which is
//!     mutable, so a derivation watermark (the last folded event id + the git
//!     rev + the dirty-tree hash) is recorded so the backbone is refutable +
//!     reproducible.
//! (c) Non-rederivable (intent, decisions, rejected approaches, preferences,
//!     open problems) stays with the LLM summary — v1 does not shrink that
//!     path.
//!
//! Conflict rate (a free measurement under v1's add-only coexistence): the
//! LLM summary may fabricate a touched file (recall a path that was never touched). The merge counts those
//! fabrications against the backbone's ground-truth file set, producing a per-
//! compact conflict rate that is the v2 signal for shrinking the LLM path.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use houyicoder_context::{EventId, TurnEvent, TurnEventKind};

/// A host-side workspace probe for the derivation watermark. The git rev +
/// dirty-tree hash come from the live workspace, which is mutable, so the
/// backbone records them as a watermark (the snapshot is point-in-time). Both
/// methods are best-effort: None when the cwd is not a git repo or git is
/// unavailable, so a probe failure never fails a compaction.
pub trait WorkspaceProbe: Send + Sync {
    /// The current git revision (HEAD), or None when not a git repo.
    fn git_rev(&self) -> Option<String>;
    /// A hash of the dirty-tree porcelain (git status --porcelain), or None.
    fn dirty_tree_hash(&self) -> Option<String>;
}

/// A workspace probe that runs host-side git against a shared cwd handle so a
/// worktree switch propagates. Matches the host-side git probe pattern (not a
/// sandboxed tool run): the spawn is allowed here as it is for the worktree
/// controller's git probing, because the backbone is a compaction-internal
/// read, not an agent-driven command.
pub struct GitWorkspaceProbe {
    cwd: Arc<std::sync::RwLock<PathBuf>>,
}

impl GitWorkspaceProbe {
    /// Construct with the runner's shared cwd handle so a worktree switch is
    /// visible at the next probe.
    pub fn new(cwd: Arc<std::sync::RwLock<PathBuf>>) -> Self {
        Self { cwd }
    }

    /// Host-side git probing (not a sandboxed tool run), so the spawn-
    /// chokepoint rule is allowed here as it is for the worktree controller's
    /// git probing — the backbone is a compaction-internal read, not an
    /// agent-driven command.
    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    fn run_git(&self, args: &[&str]) -> Option<String> {
        let cwd = self.cwd.read().ok()?;
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd.as_os_str())
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

impl WorkspaceProbe for GitWorkspaceProbe {
    fn git_rev(&self) -> Option<String> {
        self.run_git(&["rev-parse", "HEAD"])
            .filter(|s| !s.is_empty())
    }

    fn dirty_tree_hash(&self) -> Option<String> {
        let porcelain = self.run_git(&["status", "--porcelain"])?;
        if porcelain.is_empty() {
            // A clean tree: hash the empty string so a clean + a dirty tree
            // never collide, and two clean trees agree.
            return Some(format!("{:x}", md5_empty()));
        }
        Some(format!("{:x}", simple_hash(&porcelain)))
    }
}

/// A test probe returning fixed values so the watermark path is exercisable
/// without a real git repo.
pub struct StubWorkspaceProbe {
    pub rev: Option<String>,
    pub hash: Option<String>,
}

impl WorkspaceProbe for StubWorkspaceProbe {
    fn git_rev(&self) -> Option<String> {
        self.rev.clone()
    }
    fn dirty_tree_hash(&self) -> Option<String> {
        self.hash.clone()
    }
}

/// The re-derivable backbone carried alongside the LLM summary.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactBackbone {
    /// File paths touched by read/write/edit/multiedit tool calls in the
    /// folded span, sorted + deduped. Read-only discovery tools (glob/grep)
    /// are excluded; bash command text is not parsed in v1.
    pub file_touch_set: Vec<String>,
    /// Test runs detected from bash commands + their results.
    pub test_results: Vec<TestResult>,
    /// The last todo_write snapshot re-derived from the folded span, or None
    /// when no todo_write fired in the folded span.
    pub todo_snapshot: Option<Vec<TodoLine>>,
    /// The derivation watermark: the last folded event id (log anchor) + the
    /// workspace revision + dirty-tree hash (point-in-time, mutable source).
    pub derivation: BackboneDerivation,
}

/// The watermark making the workspace-rederivable part refutable + reproducible.
#[derive(Debug, Clone, PartialEq)]
pub struct BackboneDerivation {
    /// The last folded event id — the log anchor (immutable source).
    pub last_folded_event_id: EventId,
    /// The git revision at compact time, or None when no probe / not a repo.
    pub workspace_revision: Option<String>,
    /// A hash of the dirty-tree porcelain, or None when no probe.
    pub dirty_tree_hash: Option<String>,
}

/// One test run re-derived from a bash tool call + its result.
#[derive(Debug, Clone, PartialEq)]
pub struct TestResult {
    pub command: String,
    pub outcome: TestOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestOutcome {
    Pass,
    Fail,
    Unknown,
}

/// One todo line re-derived from the last todo_write in the folded span.
#[derive(Debug, Clone, PartialEq)]
pub struct TodoLine {
    pub content: String,
    pub status: String,
}

/// Derive the backbone from the folded (Summarized) events. A pure function of
/// the events + the probe: re-deriving from the same log yields the same
/// backbone (the no-drift invariant). The probe contributes only the
/// workspace watermark; None probe ⇒ the watermark fields are None (layer a
/// only).
pub fn derive_backbone(
    events: &[TurnEvent],
    folded_ids: &HashSet<EventId>,
    probe: Option<&dyn WorkspaceProbe>,
) -> CompactBackbone {
    let folded: Vec<&TurnEvent> = events
        .iter()
        .filter(|e| folded_ids.contains(&e.id))
        .collect();

    let mut file_touch_set: Vec<String> = Vec::new();
    let mut test_results: Vec<TestResult> = Vec::new();
    let mut last_todo: Option<Vec<TodoLine>> = None;

    // Pair ToolCall ↔ ToolResult by call_id so a bash test run can read its
    // own exit code + stdout.
    let mut results_by_call: std::collections::HashMap<&str, &TurnEvent> =
        std::collections::HashMap::new();
    for ev in &folded {
        if let TurnEventKind::ToolResult { call_id, .. } = &ev.kind {
            results_by_call.insert(call_id.as_str(), ev);
        }
    }

    for ev in &folded {
        let TurnEventKind::ToolCall {
            call_id,
            tool,
            input,
        } = &ev.kind
        else {
            continue;
        };
        match tool.as_str() {
            "read" | "write" | "edit" | "multiedit" => {
                if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                    let path = path.to_string();
                    if !file_touch_set.contains(&path) {
                        file_touch_set.push(path);
                    }
                }
            }
            "bash" => {
                if let Some(cmd) = input.get("command").and_then(|v| v.as_str())
                    && looks_like_test(cmd)
                {
                    let outcome = results_by_call
                        .get(call_id.as_str())
                        .map(|r| test_outcome(r))
                        .unwrap_or(TestOutcome::Unknown);
                    test_results.push(TestResult {
                        command: cmd.to_string(),
                        outcome,
                    });
                }
            }
            "todo_write" => {
                // Prefer the matching ToolResult's todos (captures the
                // all-done-clears state); fall back to the call's input.
                let todos = results_by_call
                    .get(call_id.as_str())
                    .and_then(|r| todos_from_result(r))
                    .or_else(|| todos_from_call(input));
                if let Some(todos) = todos {
                    last_todo = Some(todos);
                }
            }
            _ => {}
        }
    }

    file_touch_set.sort();
    file_touch_set.dedup();

    let last_folded_event_id = folded.last().map(|e| e.id).unwrap_or_default();
    let (workspace_revision, dirty_tree_hash) = match probe {
        Some(p) => (p.git_rev(), p.dirty_tree_hash()),
        None => (None, None),
    };

    CompactBackbone {
        file_touch_set,
        test_results,
        todo_snapshot: last_todo,
        derivation: BackboneDerivation {
            last_folded_event_id,
            workspace_revision,
            dirty_tree_hash,
        },
    }
}

/// Render the backbone as a structured text block labeled derived-from-log,
/// placed after the LLM narrative. The label marks it authoritative on
/// conflict: a fabricated path in the LLM summary does not appear here.
pub fn render_backbone_block(b: &CompactBackbone) -> String {
    let mut out = String::new();
    out.push_str("--- derived-from-log (authoritative) ---\n");
    if !b.file_touch_set.is_empty() {
        out.push_str("Files touched:\n");
        for p in &b.file_touch_set {
            out.push_str(&format!("- {p}\n"));
        }
    }
    if !b.test_results.is_empty() {
        out.push_str("Test results:\n");
        for t in &b.test_results {
            out.push_str(&format!("- [{}] {}\n", t.outcome.label(), t.command));
        }
    }
    if let Some(todos) = &b.todo_snapshot {
        out.push_str("Todo snapshot:\n");
        for line in todos {
            out.push_str(&format!("- [{}] {}\n", line.status, line.content));
        }
    }
    out.push_str(&format!(
        "Derivation: event={} rev={} dirty={}\n",
        b.derivation.last_folded_event_id,
        b.derivation.workspace_revision.as_deref().unwrap_or("none"),
        b.derivation.dirty_tree_hash.as_deref().unwrap_or("none"),
    ));
    out
}

/// The conflict-rate measurement: file paths the LLM summary mentions that are
/// NOT in the backbone's ground-truth file-touch set (fabrications). v1 counts
/// fabrications only; omissions are not errors (the LLM may legitimately elide
/// a touched file). The rate is fabrications over the file-touch-set size so a
/// large fabrication against a small backbone scores high.
#[derive(Debug, Clone)]
pub struct ConflictRate {
    pub rederivable_files: usize,
    pub llm_fabrications: usize,
    pub rate: f64,
}

/// Merge the LLM summary with the backbone block + measure the conflict rate.
/// The merged text places the backbone block after the LLM narrative; the
/// backbone is labeled authoritative, so on conflict (the LLM fabricated a
/// touched file) the backbone's real set wins.
pub fn merge_summary(llm_summary: &str, backbone: &CompactBackbone) -> (String, ConflictRate) {
    let mentioned = path_like_tokens(llm_summary);
    let touch_set: HashSet<&str> = backbone.file_touch_set.iter().map(|s| s.as_str()).collect();
    let fabrications: Vec<String> = mentioned
        .into_iter()
        .filter(|m| !touch_set.contains(m.as_str()))
        .collect();
    let rederivable_files = backbone.file_touch_set.len();
    let rate = if rederivable_files == 0 {
        if fabrications.is_empty() { 0.0 } else { 1.0 }
    } else {
        fabrications.len() as f64 / rederivable_files as f64
    };
    let block = render_backbone_block(backbone);
    let merged = if llm_summary.is_empty() {
        block
    } else {
        format!("{llm_summary}\n\n{block}")
    };
    (
        merged,
        ConflictRate {
            rederivable_files,
            llm_fabrications: fabrications.len(),
            rate,
        },
    )
}

impl TestOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unknown => "unknown",
        }
    }
}

/// Detect a test-invoking bash command. Conservative: matches the common test
/// runners so a non-test command rarely lands here.
fn looks_like_test(command: &str) -> bool {
    let lower = command.to_lowercase();
    lower.contains("cargo test")
        || lower.contains("cargo nextest")
        || lower.contains("pytest")
        || lower.contains("npm test")
        || lower.contains("npm run test")
        || lower.contains("jest")
        || lower.contains("make test")
        || lower.contains("make check")
}

/// Infer a test outcome from the bash ToolResult: Fail on a non-zero exit or a
/// FAILED marker in stdout; Pass on a zero exit with no FAILED; Unknown
/// otherwise.
fn test_outcome(result: &TurnEvent) -> TestOutcome {
    let TurnEventKind::ToolResult { output, .. } = &result.kind else {
        return TestOutcome::Unknown;
    };
    let exit_code = output.get("exit_code").and_then(|v| v.as_i64());
    let stdout = output.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
    let failed = stdout.to_lowercase().contains("failed");
    match exit_code {
        Some(0) if !failed => TestOutcome::Pass,
        Some(0) => TestOutcome::Fail,
        Some(_) => TestOutcome::Fail,
        None => {
            if failed {
                TestOutcome::Fail
            } else {
                TestOutcome::Unknown
            }
        }
    }
}

/// Read the todos array from a todo_write ToolResult output (captures the
/// all-done-clears state the call input does not).
fn todos_from_result(result: &TurnEvent) -> Option<Vec<TodoLine>> {
    let TurnEventKind::ToolResult { output, .. } = &result.kind else {
        return None;
    };
    let todos = output.get("todos")?.as_array()?;
    parse_todos(todos)
}

/// Read the todos array from a todo_write ToolCall input.
fn todos_from_call(input: &serde_json::Value) -> Option<Vec<TodoLine>> {
    let todos = input.get("todos")?.as_array()?;
    parse_todos(todos)
}

fn parse_todos(todos: &[serde_json::Value]) -> Option<Vec<TodoLine>> {
    let mut out = Vec::with_capacity(todos.len());
    for t in todos {
        let content = t.get("content").and_then(|v| v.as_str())?.to_string();
        let status = t
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("pending")
            .to_string();
        out.push(TodoLine { content, status });
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Extract file-path-like tokens from free text: runs of word/dash/dot/slash
/// characters that end in a short alphanumeric extension (e.g. real.rs,
/// src/foo.toml). Catches the paths an LLM summary would name; misses pure
/// prose (no false fabrications from narrative).
fn path_like_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in text.split_whitespace() {
        let trimmed = tok.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '/' && c != '.' && c != '-' && c != '_'
        });
        if is_path_like(trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn is_path_like(s: &str) -> bool {
    // Must contain a dot-extension (alpha-only, 1-5 chars) so prose like
    // "section" does not match, OR contain a slash (a directory path).
    let has_slash = s.contains('/');
    let has_ext = {
        let dot = match s.rfind('.') {
            Some(i) => i,
            None => false as usize,
        };
        if dot == 0 {
            false
        } else {
            let ext = &s[dot + 1..];
            !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
    };
    (has_slash || has_ext) && s.chars().any(|c| c.is_alphanumeric()) && s.len() <= 256
}

/// A stable, cheap string hash for the dirty-tree porcelain (the value only
/// needs to agree across re-derivation of the same porcelain, not be
/// cryptographically strong).
fn simple_hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn md5_empty() -> u64 {
    simple_hash("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{EventId, SessionId, TurnEvent, TurnEventKind};
    use serde_json::json;

    fn make_event(kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    fn folded_ids(events: &[TurnEvent]) -> HashSet<EventId> {
        events.iter().map(|e| e.id).collect()
    }

    fn tool_call(call_id: &str, tool: &str, input: serde_json::Value) -> TurnEvent {
        make_event(TurnEventKind::ToolCall {
            call_id: call_id.into(),
            tool: tool.into(),
            input,
        })
    }

    fn tool_result(call_id: &str, output: serde_json::Value) -> TurnEvent {
        make_event(TurnEventKind::ToolResult {
            call_id: call_id.into(),
            output,
            duration_ms: 0,
        })
    }

    #[test]
    fn test_derive_extracts_touch_set() {
        let e1 = tool_call("c1", "read", json!({"path": "src/b.rs"}));
        let e2 = tool_call("c2", "write", json!({"path": "src/a.rs", "content": "x"}));
        let e3 = tool_call(
            "c3",
            "edit",
            json!({"path": "src/a.rs", "old_string": "x", "new_string": "y"}),
        );
        let events = vec![e1, e2, e3];
        let ids = folded_ids(&events);
        let b = derive_backbone(&events, &ids, None);
        assert_eq!(
            b.file_touch_set,
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
    }

    #[test]
    fn test_derive_excludes_discovery_tools() {
        // glob/grep are read-only discovery — their path is a scope/pattern,
        // not a touched file, so they do not enter the file-touch set.
        let e1 = tool_call("c1", "glob", json!({"pattern": "**/*.rs", "path": "src"}));
        let e2 = tool_call("c2", "grep", json!({"pattern": "foo", "path": "src"}));
        let events = vec![e1, e2];
        let ids = folded_ids(&events);
        let b = derive_backbone(&events, &ids, None);
        assert!(b.file_touch_set.is_empty(), "discovery tools excluded");
    }

    #[test]
    fn test_derive_extracts_test_results() {
        let pass = tool_call("c1", "bash", json!({"command": "cargo test"}));
        let pass_r = tool_result(
            "c1",
            json!({"stdout": "test result: ok", "exit_code": 0, "success": true}),
        );
        let fail = tool_call("c2", "bash", json!({"command": "cargo test"}));
        let fail_r = tool_result(
            "c2",
            json!({"stdout": "FAILED", "exit_code": 1, "success": false}),
        );
        let events = vec![pass, pass_r, fail, fail_r];
        let ids = folded_ids(&events);
        let b = derive_backbone(&events, &ids, None);
        assert_eq!(b.test_results.len(), 2);
        assert_eq!(b.test_results[0].outcome, TestOutcome::Pass);
        assert_eq!(b.test_results[1].outcome, TestOutcome::Fail);
    }

    #[test]
    fn test_derive_todo_last_write() {
        let first = tool_call(
            "c1",
            "todo_write",
            json!({"todos": [{"content": "old", "status": "completed"}]}),
        );
        let first_r = tool_result(
            "c1",
            json!({"todos": [{"content": "old", "status": "completed"}]}),
        );
        let second = tool_call(
            "c2",
            "todo_write",
            json!({"todos": [{"content": "new", "status": "in_progress"}]}),
        );
        let second_r = tool_result(
            "c2",
            json!({"todos": [{"content": "new", "status": "in_progress"}]}),
        );
        let events = vec![first, first_r, second, second_r];
        let ids = folded_ids(&events);
        let b = derive_backbone(&events, &ids, None);
        let todos = b.todo_snapshot.expect("snapshot present");
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].content, "new");
    }

    #[test]
    fn test_log_rederivable_no_staleness() {
        // Re-deriving from the same log + ids yields an identical backbone —
        // the no-drift invariant of the append-only log layer.
        let e1 = tool_call(
            "c1",
            "edit",
            json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
        );
        let e2 = tool_call("c2", "bash", json!({"command": "cargo test"}));
        let e3 = tool_result(
            "c2",
            json!({"stdout": "ok", "exit_code": 0, "success": true}),
        );
        let events = vec![e1, e2, e3];
        let ids = folded_ids(&events);
        let b1 = derive_backbone(&events, &ids, None);
        let b2 = derive_backbone(&events, &ids, None);
        assert_eq!(b1, b2, "re-derivation is deterministic");
    }

    #[test]
    fn test_workspace_watermark_recorded() {
        let e1 = tool_call(
            "c1",
            "edit",
            json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
        );
        let events = vec![e1];
        let ids = folded_ids(&events);
        let probe = StubWorkspaceProbe {
            rev: Some("abc123".into()),
            hash: Some("deadbeef".into()),
        };
        let b = derive_backbone(&events, &ids, Some(&probe));
        assert_eq!(b.derivation.workspace_revision.as_deref(), Some("abc123"));
        assert_eq!(b.derivation.dirty_tree_hash.as_deref(), Some("deadbeef"));
        assert_eq!(b.derivation.last_folded_event_id, events[0].id);
    }

    #[test]
    fn test_merge_appends_backbone_llm() {
        // v1 is add-only: the merged text has BOTH the LLM summary + the
        // derived-from-log backbone block.
        let backbone = CompactBackbone {
            file_touch_set: vec!["real.rs".into()],
            test_results: vec![],
            todo_snapshot: None,
            derivation: BackboneDerivation {
                last_folded_event_id: EventId::new(),
                workspace_revision: None,
                dirty_tree_hash: None,
            },
        };
        let (merged, _conflict) = merge_summary("summary text", &backbone);
        assert!(merged.contains("summary text"), "LLM summary kept");
        assert!(
            merged.contains("derived-from-log"),
            "backbone block appended"
        );
        assert!(
            merged.contains("real.rs"),
            "backbone lists the touched file"
        );
    }

    #[test]
    fn test_merge_flags_fabricated_path() {
        // The LLM summary mentions a path never touched (a fabrication) — the
        // conflict rate must reflect it, and the backbone block lists only the
        // real set (backbone wins).
        let backbone = CompactBackbone {
            file_touch_set: vec!["real.rs".into()],
            test_results: vec![],
            todo_snapshot: None,
            derivation: BackboneDerivation {
                last_folded_event_id: EventId::new(),
                workspace_revision: None,
                dirty_tree_hash: None,
            },
        };
        let (merged, conflict) = merge_summary("we edited fake_file.rs here", &backbone);
        assert!(conflict.llm_fabrications >= 1, "fabrication counted");
        assert!(conflict.rate > 0.0, "rate > 0");
        // Add-only: the LLM narrative is kept verbatim (the fabrication stays
        // in the LLM section — that is what the conflict rate measures). The
        // backbone block (authoritative) lists the REAL set, not the
        // fabrication: backbone wins on conflict.
        assert!(
            merged.contains("derived-from-log"),
            "backbone block present"
        );
        let block = merged
            .split("--- derived-from-log")
            .nth(1)
            .expect("block after the marker");
        assert!(block.contains("real.rs"), "backbone lists the real path");
        assert!(
            !block.contains("fake_file.rs"),
            "backbone does not carry the fabrication"
        );
    }

    #[test]
    fn test_merge_path_no_conflict() {
        // The LLM mentions a real touched path — no fabrication, zero rate.
        let backbone = CompactBackbone {
            file_touch_set: vec!["real.rs".into()],
            test_results: vec![],
            todo_snapshot: None,
            derivation: BackboneDerivation {
                last_folded_event_id: EventId::new(),
                workspace_revision: None,
                dirty_tree_hash: None,
            },
        };
        let (_merged, conflict) = merge_summary("we edited real.rs", &backbone);
        assert_eq!(
            conflict.llm_fabrications, 0,
            "real path is not a fabrication"
        );
        assert!((conflict.rate - 0.0).abs() < 1e-9, "zero rate");
    }
}
