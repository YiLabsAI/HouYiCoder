//! Pure render functions for the slash-command output lines: /status,
//! /sandbox, /trajectory, /tools. Split from command.rs so the
//! dispatch surface stays under the file-size gate. Each fn takes a snapshot
//! or record slice and returns a plain string the host prints — no Frame, no
//! ratatui, so the layering stays Presentation -> core (the host composes the
//! string; the render fns never name a resilience or provider type directly).

use houyicoder_protocol::frontend::permission::{PermissionEffect, PermissionMode};
use houyicoder_protocol::frontend::status::StatusSnapshot;

/// The display label for a wire permission mode, so the render path never
/// imports the permission crate (the server is the mode authority).
pub(crate) fn permission_mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Manual => "manual",
        PermissionMode::Auto => "auto",
        // non_exhaustive: an unknown wire variant labels as manual (fail-safe).
        _ => "manual",
    }
}

/// The display label for a wire rule effect.
pub(crate) fn permission_effect_label(effect: PermissionEffect) -> &'static str {
    match effect {
        PermissionEffect::Allow => "allow",
        PermissionEffect::Reject => "reject",
        PermissionEffect::Ask => "ask",
    }
}

/// A safe percentage (None when the denominator is zero, so /context never
/// divides by zero on a provider that reports no window).
fn pct(numerator: u32, denominator: u32) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        Some(100.0 * numerator as f64 / denominator as f64)
    }
}

/// /context: the precise 7-field cumulative usage from the provider (no
/// chars/4 estimate), the current window footprint, and the model. Cache and
/// reasoning breakdowns are first-class so the cache hit rate and the visible
/// vs reasoning split are visible at a glance.
pub(crate) fn render_context(snap: &StatusSnapshot) -> String {
    let u = &snap.cumulative_usage;
    let window = match pct(snap.last_input_tokens, snap.context_window) {
        Some(p) => format!(
            "{}/{} ({:.1}%)",
            snap.last_input_tokens, snap.context_window, p
        ),
        None => "0 (no provider window)".to_string(),
    };
    let mut s = String::new();
    s.push_str(&format!("model: {}\n", snap.model));
    s.push_str(&format!("window: {}\n", window));
    s.push_str("cumulative (this session):\n");
    s.push_str(&format!(
        "  input        {}  (cache_read {} · non_cached {})\n",
        u.input_tokens, u.cache_read_input_tokens, u.non_cached_input_tokens
    ));
    s.push_str(&format!(
        "  output       {}  (reasoning {} · visible {})\n",
        u.output_tokens,
        u.reasoning_tokens,
        u.visible_output_tokens()
    ));
    s.push_str(&format!("  cache_write  {}\n", u.cache_write_input_tokens));
    s.push_str(&format!("  total        {}\n", u.total_tokens));
    s.push_str(&format!(
        "tools         {} calls  ({} ok · {} errored)",
        snap.tool_calls, snap.tool_success, snap.tool_errors
    ));
    s
}

/// /tools: the registered tool set, one row per tool — name + description.
/// Capability discoverability: the user (and host) can see what the agent can
/// do without reading source. Empty when no runner is wired.
pub(crate) fn render_tools(tools: &[houyicoder_protocol::frontend::tools::ToolEntry]) -> String {
    if tools.is_empty() {
        return "tools: none registered (no runner wired)".to_string();
    }
    let mut s = format!("tools: {} registered\n", tools.len());
    let mut sorted: Vec<&houyicoder_protocol::frontend::tools::ToolEntry> = tools.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in sorted {
        let one = entry
            .description
            .lines()
            .next()
            .filter(|l| !l.is_empty())
            .unwrap_or("(no description)");
        s.push_str(&format!("  {:<16} {}\n", entry.name, one));
    }
    s
}

/// Render one memory's full body (the /memory <key> show reply): the
/// frontmatter header (source, key, description) followed by the body content.
pub(crate) fn render_memory_entry(
    entry: &houyicoder_protocol::frontend::memory::MemoryDetail,
) -> String {
    let mut s = format!("memory: [{}] {}\n", entry.source, entry.key);
    if !entry.description.is_empty() {
        s.push_str(&format!(
            "  {}\n",
            entry.description.lines().next().unwrap_or("")
        ));
    }
    s.push_str(&entry.content);
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

use crate::evidence::MemoryEntry;
/// Map wire memory summaries to the TUI pane rows: topic = key, summary =
/// "[source] description" (or "[source]" when the description is empty). Pure
/// over the wire slice so the mapping is unit-testable independent of the App
/// plumbing that feeds it (the App handler just calls this + sets the pane).
use crate::state::enums::MemoryScopeTab;

/// Filter the memory list by the active scope tab + a text search substring
/// (key + description, case-insensitive). All scope returns every entry; the
/// others narrow to one physical root. An empty search string skips the text
/// filter. Shared by the render path + the d/enter actions so the cursor index
/// the user sees is the same one the action targets (no drift between render +
/// act).
pub(crate) fn filtered_memory<'a>(
    entries: &'a [MemoryEntry],
    tab: MemoryScopeTab,
    search: &str,
) -> Vec<&'a MemoryEntry> {
    let needle = search.to_ascii_lowercase();
    entries
        .iter()
        .filter(|m| tab == MemoryScopeTab::All || m.scope == tab.label())
        .filter(|m| {
            needle.is_empty()
                || m.topic.to_ascii_lowercase().contains(&needle)
                || m.summary.to_ascii_lowercase().contains(&needle)
        })
        .collect()
}

pub(crate) fn memory_entries_from_wire(
    entries: &[houyicoder_protocol::frontend::memory::MemorySummaryEntry],
) -> Vec<crate::evidence::MemoryEntry> {
    entries
        .iter()
        .map(|e| crate::evidence::MemoryEntry {
            topic: e.key.clone(),
            summary: if e.description.is_empty() {
                String::new()
            } else {
                e.description.clone()
            },
            scope: e.scope.clone(),
            source: e.source.clone(),
        })
        .collect()
}

/// The checklist section folded into /status: every item grouped by status
/// with a counts header. Empty when no checklist tool is wired or the list is
/// cleared, so /status stays clean on a fresh turn. Reads the tool's shared
/// state handle (lock and clone), never dispatching a tool call.
pub(crate) fn render_todo_section(todos: &[crate::todo_view::TodoView]) -> String {
    use crate::todo_view::TodoStatus;
    if todos.is_empty() {
        return String::new();
    }
    let done = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let active = todos
        .iter()
        .filter(|t| t.status == TodoStatus::InProgress)
        .count();
    let open = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Pending)
        .count();
    let mut s = format!(
        "tasks: {} ({} done, {} in progress, {} open)\n",
        todos.len(),
        done,
        active,
        open,
    );
    let glyph = |st: TodoStatus| match st {
        TodoStatus::InProgress => "◼",
        TodoStatus::Completed => "✔",
        TodoStatus::Pending => "◻",
    };
    let mut grouped: Vec<&crate::todo_view::TodoView> = Vec::with_capacity(todos.len());
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::InProgress));
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::Pending));
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::Completed));
    for t in grouped {
        let label = if t.status == TodoStatus::InProgress {
            t.active_form.clone().unwrap_or_else(|| t.content.clone())
        } else {
            t.content.clone()
        };
        s.push_str(&format!("  {} {}\n", glyph(t.status), label));
    }
    s
}

/// /permissions view: the full permission surface — active mode, durable
/// rules, and the session verdict log. The verdict log projects
/// PermissionDecision TurnEvents (the durable audit trail of every approve /
/// deny the human issued this session). One row per verdict: the verdict,
/// the tool, the scope, the call_id, and the wall-clock ts.
pub(crate) fn render_permission_view(
    mode: houyicoder_protocol::frontend::permission::PermissionMode,
    rules: &[houyicoder_protocol::frontend::permission::PermissionRule],
    verdicts: &[houyicoder_protocol::frontend::permission::PermissionDecisionEntry],
    ask_before_git: bool,
) -> String {
    let mut s = format!("mode: {}\n", permission_mode_label(mode));
    s.push_str(&format!(
        "ask before git operations: {} (git commit/rebase/reset/tag {} before running)\n",
        if ask_before_git { "on" } else { "off" },
        if ask_before_git {
            "ask"
        } else {
            "run without asking"
        },
    ));
    s.push_str("rules:");
    if rules.is_empty() {
        s.push_str(" (none — mode defaults apply)\n");
    } else {
        for (i, r) in rules.iter().enumerate() {
            s.push_str(&format!(
                "\n  [{i}] {} {}",
                r.action,
                permission_effect_label(r.effect)
            ));
        }
        s.push('\n');
    }
    s.push_str("verdicts:");
    if verdicts.is_empty() {
        s.push_str(" (none this session)\n");
    } else {
        for v in verdicts {
            s.push_str(&format!(
                "\n  {} {} ({}) call_id:{}",
                v.verdict, v.tool, v.scope, v.call_id,
            ));
        }
        s.push('\n');
    }
    s
}

/// /status: identity block first (version / session name / session id / cwd
/// from the sidecar), then the runtime block (model / mode / sandbox /
/// breaker) the host's own gate completes. The Session section matches the
/// cost-tracker layout (duration / usage / tasks); fields not tracked yet
/// (USD cost, API duration, code-change lines, per-model split) are deferred
/// to the tabbed-panel track, not faked. When the sidecar is absent (the
/// stub and test path, before the wire returns a sidecar), the identity
/// lines drop and only the runtime block renders so the command never shows
/// blank fields.
pub(crate) fn field(label: &str, value: &str) -> String {
    format!("{:<22}{value}\n", format!("{label}:"))
}

/// Compact token count with k/m suffixes (1.3k / 1.6m) so the Usage tab's
/// per-model rows fit one line. A compact formatter:
/// fewer than 1000 stays raw, thousands use one-decimal k, millions use
/// one-decimal m. A trailing .0 is stripped (1.0k -> 1k) so the
/// chip stays tight.
pub(crate) fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        trim_zero(format!("{:.1}m", n as f64 / 1_000_000.0))
    } else if n >= 1000 {
        trim_zero(format!("{:.1}k", n as f64 / 1000.0))
    } else {
        n.to_string()
    }
}

fn trim_zero(s: String) -> String {
    s.replacen(".0", "", 1)
}

pub(crate) fn render_status(
    snap: &StatusSnapshot,
    session: &houyicoder_protocol::frontend::SessionId,
    sandbox: &str,
    todos: &[crate::todo_view::TodoView],
) -> String {
    let mut s = String::new();
    // Identity fields, in display order: Version, Session name, Session ID,
    // cwd, Auth token, Anthropic base URL, Model, sandbox, Setting sources.
    // Version is the running build (always known, set by the server on the
    // snapshot itself); name/cwd/provenance come from the sidecar and drop
    // honestly when it is not materialized yet. The Session name row is
    // spliced into an editable line by the pane when the user presses e.
    s.push_str(&field("Version", &snap.version));
    let name = snap
        .meta
        .as_ref()
        .and_then(|m| m.name.as_deref())
        .unwrap_or("(unnamed)");
    s.push_str(&field("Session name", name));
    s.push_str(&field("Session ID", &session.to_string()));
    if let Some(meta) = snap.meta.as_ref() {
        s.push_str(&field("cwd", &meta.cwd));
    }
    s.push_str(&field(
        "Auth token",
        snap.auth_token_source.as_deref().unwrap_or("(none)"),
    ));
    s.push_str(&field("Anthropic base URL", &snap.base_url));
    s.push_str(&field("Model", &snap.model));
    s.push_str(&field("sandbox", sandbox));
    s.push_str(&field("Setting sources", &snap.setting_sources));
    if let Some(meta) = snap.meta.as_ref() {
        s.push_str(&field("provenance", &render_provenance(&meta.provenance)));
    }
    // Todos (appended; tokens + wall duration live in the Usage tab).
    let todo = render_todo_section(todos);
    if !todo.is_empty() {
        s.push_str(&todo);
    }
    s
}

/// Render the session provenance as a compact one-line label. Fresh = a new
/// session; ForkedFrom = split off an existing session (the origin sid +
/// optional turn seq); ResumedFromExport = bootstrapped from an exported
/// transcript file. The origin sid is shown short so the line fits.
fn render_provenance(p: &houyicoder_protocol::frontend::status::SessionProvenance) -> String {
    use houyicoder_protocol::frontend::status::SessionProvenance as P;
    match p {
        P::Fresh => "fresh".to_string(),
        P::ForkedFrom { from_sid, from_seq } => match from_seq {
            Some(seq) => format!("forked from {from_sid} at turn {seq}"),
            None => format!("forked from {from_sid}"),
        },
        P::ResumedFromExport { source_session_id } => {
            format!("resumed from export {source_session_id}")
        }
        P::SpawnedBy {
            parent_session_id,
            subagent_type,
            task_id: _,
        } => format!("spawned by {parent_session_id} ({subagent_type})"),
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::todo_view::{TodoStatus, TodoView};
    use houyicoder_protocol::frontend::status::{
        SessionMetaSummary, SessionProvenance, StatusSnapshot,
    };
    use houyicoder_protocol::llm::Usage;

    fn snap(model: &str) -> StatusSnapshot {
        StatusSnapshot {
            model: model.into(),
            breaker_state: None,
            breaker_reason: None,
            breaker_cool_down_secs: None,
            cumulative_usage: Usage {
                input_tokens: 100,
                output_tokens: 40,
                total_tokens: 140,
                non_cached_input_tokens: 10,
                cache_read_input_tokens: 80,
                cache_write_input_tokens: 10,
                reasoning_tokens: 5,
            },
            last_input_tokens: 100,
            context_window: 200_000,
            tool_calls: 3,
            tool_success: 2,
            tool_errors: 1,
            meta: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            ..Default::default()
        }
    }

    fn snap_with_meta(model: &str) -> StatusSnapshot {
        let mut s = snap(model);
        s.meta = Some(SessionMetaSummary {
            name: Some("fix login bug".to_string()),
            cwd: "/work/app".to_string(),
            model: model.into(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            provenance: SessionProvenance::Fresh,
        });
        s
    }

    fn item(content: &str, status: TodoStatus) -> TodoView {
        TodoView {
            content: content.into(),
            status,
            active_form: None,
        }
    }

    #[test]
    fn test_todo_section_empty_cleared() {
        assert!(render_todo_section(&[]).is_empty());
    }

    #[test]
    fn test_todo_section_lists_glyphs() {
        let todos = vec![
            item("done", TodoStatus::Completed),
            item("now", TodoStatus::InProgress),
            item("later", TodoStatus::Pending),
        ];
        let out = render_todo_section(&todos);
        assert!(out.contains("tasks: 3 (1 done, 1 in progress, 1 open)"));
        // Grouped order: in-progress first, then pending, then completed.
        let now_pos = out.find("◼ now").unwrap();
        let later_pos = out.find("◻ later").unwrap();
        let done_pos = out.find("✔ done").unwrap();
        assert!(now_pos < later_pos && later_pos < done_pos);
    }

    #[test]
    fn test_status_renders_config_fields() {
        let s = render_status(
            &snap("glm-5.1"),
            &houyicoder_protocol::frontend::SessionId::new("test"),
            "mac-seatbelt",
            &[],
        );
        assert!(s.contains("glm-5.1") && s.contains("Model:"), "model: {s}");
        assert!(s.contains("Auth token:"), "auth token source: {s}");
        assert!(s.contains("Anthropic base URL:"), "base url: {s}");
        assert!(s.contains("Setting sources:"), "setting sources: {s}");
        assert!(s.contains("sandbox:"), "sandbox: {s}");
        // Tokens + wall duration moved to the Usage tab.
        assert!(!s.contains("tokens:"), "no tokens in status: {s}");
        assert!(
            !s.contains("tasks:"),
            "no todo section when none wired: {s}"
        );
    }

    #[test]
    fn test_status_folds_todo_present() {
        let todos = vec![item("run tests", TodoStatus::InProgress)];
        let s = render_status(
            &snap("glm-5.1"),
            &houyicoder_protocol::frontend::SessionId::new("test"),
            "mac-seatbelt",
            &todos,
        );
        assert!(s.contains("tasks: 1 (0 done, 1 in progress, 0 open)"));
        assert!(s.contains("◼ run tests"));
    }

    #[test]
    fn test_status_renders_identity_block() {
        let s = render_status(
            &snap_with_meta("glm-5.1"),
            &houyicoder_protocol::frontend::SessionId::new("sess-123"),
            "mac-seatbelt",
            &[],
        );
        // Identity order: Version, Session name, Session ID, cwd, then
        // Auth token / base URL / Model / sandbox / Setting sources, then
        // provenance last.
        let v = s.find(env!("CARGO_PKG_VERSION")).unwrap();
        let n = s.find("fix login bug").unwrap();
        let id = s.find("sess-123").unwrap();
        let cwd = s.find("/work/app").unwrap();
        let prov = s.find("provenance:").unwrap();
        assert!(
            v < n && n < id && id < cwd,
            "version < name < id < cwd: {s}"
        );
        assert!(prov > cwd, "provenance after cwd: {s}");
    }

    #[test]
    fn test_status_without_sidecar_unnamed() {
        let s = render_status(
            &snap("glm-5.1"),
            &houyicoder_protocol::frontend::SessionId::new("sess-123"),
            "mac-seatbelt",
            &[],
        );
        // No sidecar: Session name still renders (an unnamed session shows a
        // placeholder, never blank), falling to "(unnamed)". Version is the
        // running build (top-level on the snapshot, set by the server) so it
        // always renders; cwd and provenance come from the sidecar and drop.
        assert!(s.contains("(unnamed)") && s.contains("Session name:"));
        assert!(s.contains("Version:"));
        assert!(!s.contains("cwd:"));
        assert!(!s.contains("provenance:"));
        assert!(s.contains("sess-123") && s.contains("Session ID:"));
    }

    #[test]
    fn test_status_provenance_fork_format() {
        let mut s = snap_with_meta("glm-5.1");
        s.meta = s.meta.map(|mut m| {
            m.provenance = SessionProvenance::ForkedFrom {
                from_sid: "sess-aaa".into(),
                from_seq: Some(7),
            };
            m
        });
        let out = render_status(
            &s,
            &houyicoder_protocol::frontend::SessionId::new("sess-123"),
            "mac-seatbelt",
            &[],
        );
        assert!(out.contains("forked from sess-aaa at turn 7") && out.contains("provenance:"));
    }

    #[test]
    fn test_status_provenance_spawned_format() {
        let mut s = snap_with_meta("glm-5.1");
        s.meta = s.meta.map(|mut m| {
            m.provenance = SessionProvenance::SpawnedBy {
                parent_session_id: "parent-1".into(),
                subagent_type: "explore".into(),
                task_id: "task-7".into(),
            };
            m
        });
        let out = render_status(
            &s,
            &houyicoder_protocol::frontend::SessionId::new("sess-123"),
            "mac-seatbelt",
            &[],
        );
        assert!(out.contains("spawned by parent-1 (explore)") && out.contains("provenance:"));
    }

    #[test]
    fn test_status_provenance_resumed_format() {
        let mut s = snap_with_meta("glm-5.1");
        s.meta = s.meta.map(|mut m| {
            m.provenance = SessionProvenance::ResumedFromExport {
                source_session_id: "sess-orig".into(),
            };
            m
        });
        let out = render_status(
            &s,
            &houyicoder_protocol::frontend::SessionId::new("sess-123"),
            "mac-seatbelt",
            &[],
        );
        assert!(out.contains("resumed from export sess-orig") && out.contains("provenance:"));
    }

    #[test]
    fn test_status_unnamed_placeholder() {
        let mut s = snap_with_meta("glm-5.1");
        s.meta = s.meta.map(|mut m| {
            m.name = None;
            m
        });
        let out = render_status(
            &s,
            &houyicoder_protocol::frontend::SessionId::new("sess-123"),
            "mac-seatbelt",
            &[],
        );
        assert!(out.contains("(unnamed)") && out.contains("Session name:"));
    }
}

/// /sandbox: the aggregate resource fence. When Open, show the trip reason
/// and the remaining cool-down so the user knows when spawns resume. When no
/// breaker is wired (stub path), say so plainly.
pub(crate) fn render_sandbox(snap: &StatusSnapshot, sandbox: &str) -> String {
    let mut s = format!("sandbox: {}\n", sandbox);
    match (
        snap.breaker_state.as_deref(),
        &snap.breaker_reason,
        snap.breaker_cool_down_secs,
    ) {
        (Some("Open"), Some(reason), Some(remain)) => {
            s.push_str(&format!(
                "breaker: Open\n  reason: {}\n  cool-down: {}s remaining",
                reason, remain,
            ));
        }
        (Some("Open"), reason, None) => {
            s.push_str(&format!(
                "breaker: Open\n  reason: {}\n  cool-down: elapsed (probe next)",
                reason.as_deref().unwrap_or("unknown")
            ));
        }
        (Some(state), _, _) => {
            s.push_str(&format!("breaker: {}", state));
        }
        (None, _, _) => {
            s.push_str("breaker: (none wired — fence enforced by the seatbelt path)");
        }
    }
    s
}

/// The breaker line for /status: state, and when Open, the reason + cool-down.
/// The snapshot already carries render labels + a pre-rendered reason string, so
/// this function prints and never names a resilience type — the layering stays
/// Presentation -> core -> resilience.
pub(crate) fn render_breaker_line(snap: &StatusSnapshot) -> String {
    match (
        snap.breaker_state.as_deref(),
        &snap.breaker_reason,
        snap.breaker_cool_down_secs,
    ) {
        (Some("Open"), Some(reason), Some(remain)) => {
            format!("Open ({}, cool-down {}s)", reason, remain)
        }
        (Some("Open"), reason, None) => {
            format!(
                "Open ({}, cool-down elapsed)",
                reason.as_deref().unwrap_or("unknown")
            )
        }
        (Some(state), _, _) => state.to_string(),
        (None, _, _) => "(none wired)".to_string(),
    }
}

/// Render the wire trajectory (the /trajectory query reply, a Vec of
/// session/update) without importing the engine or context crate.
pub(crate) fn render_trajectory_wire(
    entries: &[houyicoder_protocol::frontend::trajectory::TrajectoryEntry],
    redundant: &[houyicoder_protocol::frontend::trajectory::RedundantCallEntry],
) -> String {
    if entries.is_empty() && redundant.is_empty() {
        return "trajectory: no events this session (resumed sessions start empty)".into();
    }
    let mut s = format!("trajectory: {} events\n", entries.len());
    for e in entries {
        let hash = e.prev_hash.clone().unwrap_or_else(|| "—".to_string());
        let id_short = if e.event_id.len() <= 8 {
            e.event_id.clone()
        } else {
            e.event_id.split_at(8).0.to_string()
        };
        let dur = e
            .duration_ms
            .map(|d| format!("  {:.1}s", d as f64 / 1000.0))
            .unwrap_or_default();
        s.push_str(&format!(
            "  {:<14} ts:{} id:{} prev:{}{}\n",
            e.kind, e.ts, id_short, hash, dur,
        ));
    }
    // Redundant calls: the self-evolution reward signal. Same-batch = the
    // model emitted the same input twice in one message (strongest); cross-
    // batch = a later call repeats a prior with no intervening write
    // (context-loss re-read). Human names, not the machine kind label.
    if !redundant.is_empty() {
        s.push_str(&format!("\nredundant calls: {} flagged\n", redundant.len()));
        for r in redundant {
            let kind = match r.kind.as_str() {
                "same-batch" => "same-message repeat",
                "cross-batch" => "cross-turn context-loss re-read",
                other => other,
            };
            s.push_str(&format!(
                "  {} {} gap={} last_seq={} preview={}\n",
                kind, r.tool, r.gap, r.last_seq, r.input_preview,
            ));
        }
    }
    s
}

/// Render the wire permission mode (the /model read reply) without
/// importing the permission crate.
pub(crate) fn render_permission_mode_wire(
    mode: houyicoder_protocol::frontend::permission::PermissionMode,
) -> String {
    format!("mode: {}", permission_mode_label(mode))
}

/// Render the wire permission rules (the /rules read reply) without
/// importing the permission crate.
pub(crate) fn render_permission_rules_wire(
    rules: &[houyicoder_protocol::frontend::permission::PermissionRule],
) -> String {
    if rules.is_empty() {
        return "rules: (none)".into();
    }
    let mut lines = vec![format!("rules ({}):", rules.len())];
    for r in rules {
        let effect = match r.effect {
            houyicoder_protocol::frontend::permission::PermissionEffect::Allow => "allow",
            houyicoder_protocol::frontend::permission::PermissionEffect::Reject => "reject",
            houyicoder_protocol::frontend::permission::PermissionEffect::Ask => "ask",
        };
        let content = match &r.content {
            Some(c) => format!(" {c:?}"),
            None => String::new(),
        };
        lines.push(format!("  {} {}{content}", r.action, effect));
    }
    lines.join("\n")
}

fn content_text(b: &houyicoder_protocol::frontend::run::ContentBlock) -> String {
    match b {
        houyicoder_protocol::frontend::run::ContentBlock::Text { text } => text.clone(),
        _ => "(non-text)".into(),
    }
}

#[cfg(test)]
#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
