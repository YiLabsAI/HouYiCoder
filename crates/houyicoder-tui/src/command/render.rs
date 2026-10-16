//! Text formatting for slash-command results.
//!
//! Pure functions map typed frontend responses to terminal text without
//! reading application state or constructing widgets.

use houyicoder_protocol::frontend::SessionId;
use houyicoder_protocol::frontend::memory::{MemoryDetail, MemorySummaryEntry};
use houyicoder_protocol::frontend::permission::{
    PermissionDecisionEntry, PermissionEffect, PermissionMode, PermissionRule,
};
use houyicoder_protocol::frontend::status::{
    SessionProvenance as WireSessionProvenance, StatusSnapshot,
};
use houyicoder_protocol::frontend::trajectory::{RedundantCallEntry, TrajectoryEntry};

use crate::evidence::MemoryEntry;
use crate::todo_view::{TodoStatus, TodoView};

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

/// /context: the precise 7-field cumulative usage from the provider (no
/// chars/4 estimate), the current window footprint, and the model. Cache and
/// reasoning breakdowns are first-class so the cache hit rate and the visible
/// vs reasoning split are visible at a glance.
///
/// The footprint is the assembled context, the same measurement the pane
/// draws, so the text form and the pane agree. A provider that reports no
/// window still gets the token count, without a share.
#[cfg(test)]
pub(crate) fn render_context(snap: &StatusSnapshot) -> String {
    let u = &snap.cumulative_usage;
    let window = match (snap.context_used_tokens, snap.context_window) {
        (Some(used), window) if window > 0 => {
            format!(
                "{used}/{window} ({:.1}%)",
                100.0 * used as f64 / window as f64
            )
        }
        (Some(used), _) => format!("{used} (no provider window)"),
        (None, _) => "not measured".to_string(),
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

/// Render one memory's full body (the /memory <key> show reply): the
/// frontmatter header (source, key, description) followed by the body content.
pub(crate) fn render_memory_entry(entry: &MemoryDetail) -> String {
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

/// Convert memory summaries into list rows without discarding recency.
pub(crate) fn memory_entries_from_wire(entries: &[MemorySummaryEntry]) -> Vec<MemoryEntry> {
    entries
        .iter()
        .map(|e| MemoryEntry {
            topic: e.key.clone(),
            summary: if e.description.is_empty() {
                String::new()
            } else {
                e.description.clone()
            },
            scope: e.scope.clone(),
            source: e.source.clone(),
            mtime_secs: e.mtime_secs,
        })
        .collect()
}

/// The checklist section folded into /status: every item grouped by status
/// with a counts header. Empty when no checklist tool is wired or the list is
/// cleared, so /status stays clean on a fresh turn. Reads the tool's shared
/// state handle (lock and clone), never dispatching a tool call.
pub(crate) fn render_todo_section(todos: &[TodoView]) -> String {
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
    let paused = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Paused)
        .count();
    let open = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Pending)
        .count();
    let mut s = format!(
        "tasks: {} ({} done, {} in progress, {} paused, {} open)\n",
        todos.len(),
        done,
        active,
        paused,
        open,
    );
    let glyph = |st: TodoStatus| match st {
        TodoStatus::InProgress => "◼",
        TodoStatus::Paused => "Ⅱ",
        TodoStatus::Completed => "✔",
        TodoStatus::Pending => "◻",
    };
    let mut grouped: Vec<&TodoView> = Vec::with_capacity(todos.len());
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::InProgress));
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::Paused));
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::Pending));
    grouped.extend(todos.iter().filter(|t| t.status == TodoStatus::Completed));
    for t in grouped {
        let label = match t.status {
            TodoStatus::InProgress => t.active_form.clone().unwrap_or_else(|| t.content.clone()),
            TodoStatus::Paused => format!("{} (paused)", t.content),
            TodoStatus::Pending | TodoStatus::Completed => t.content.clone(),
        };
        s.push_str(&format!("  {} {}\n", glyph(t.status), label));
    }
    s
}

/// /permissions view: the full permission surface — active mode, durable
/// rules, and the session verdict log. The verdict log renders
/// PermissionDecision SessionLogEntries (the durable audit trail of every approve /
/// deny the human issued this session). One row per verdict: the verdict,
/// the tool, the scope, the call_id, and the wall-clock ts.
pub(crate) fn render_permission_view(
    mode: PermissionMode,
    rules: &[PermissionRule],
    verdicts: &[PermissionDecisionEntry],
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
/// from the descriptor), then the runtime block (model / mode / sandbox /
/// breaker) the host's own gate completes. The Session section matches the
/// cost-tracker layout (duration / usage / tasks); fields not tracked yet
/// (USD cost, API duration, code-change lines, per-model split) are deferred
/// to the tabbed-panel track, not faked. When the descriptor is absent (the
/// stub and test path, before the wire returns a descriptor), the identity
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
    session: &SessionId,
    sandbox: &str,
    todos: &[TodoView],
) -> String {
    let mut s = String::new();
    s.push_str(&field("Version", &snap.version));
    let name = snap
        .descriptor
        .as_ref()
        .and_then(|descriptor| descriptor.name.as_deref())
        .unwrap_or("(unnamed)");
    s.push_str(&field("Session name", name));
    s.push_str(&field("Session ID", &session.to_string()));
    // A child's descriptor carries no cwd (nothing knows it where the child
    // is written), so an empty one drops the line instead of rendering it
    // blank.
    if let Some(descriptor) = snap.descriptor.as_ref()
        && !descriptor.cwd.is_empty()
    {
        s.push_str(&field("cwd", &descriptor.cwd));
    }
    s.push_str(&field(
        "Auth token",
        snap.auth_token_source.as_deref().unwrap_or("(none)"),
    ));
    s.push_str(&field("Base URL", &snap.base_url));
    s.push_str(&field("Model", &snap.model));
    s.push_str(&field("sandbox", sandbox));
    s.push_str(&field("Setting sources", &snap.setting_sources));
    if let Some(descriptor) = snap.descriptor.as_ref() {
        s.push_str(&field(
            "provenance",
            &render_provenance(&descriptor.provenance),
        ));
    }
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
fn render_provenance(p: &WireSessionProvenance) -> String {
    match p {
        WireSessionProvenance::Fresh => "fresh".to_string(),
        WireSessionProvenance::ForkedFrom { from_sid, from_seq } => match from_seq {
            Some(seq) => format!("forked from {from_sid} at turn {seq}"),
            None => format!("forked from {from_sid}"),
        },
        WireSessionProvenance::ResumedFromExport { source_session_id } => {
            format!("resumed from export {source_session_id}")
        }
        WireSessionProvenance::SpawnedBy {
            parent_session_id,
            subagent_type,
            task_id: _,
        } => format!("spawned by {parent_session_id} ({subagent_type})"),
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
            s.push_str("breaker: (none connected — fence enforced by the seatbelt path)");
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
        (None, _, _) => "(none connected)".to_string(),
    }
}

/// Render the wire trajectory (the /trajectory query reply, a Vec of
/// session/update) without importing the engine or context crate.
pub(crate) fn render_trajectory_wire(
    entries: &[TrajectoryEntry],
    redundant: &[RedundantCallEntry],
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

/// Render the wire permission rules (the /rules read reply) without
/// importing the permission crate.
pub(crate) fn render_permission_rules_wire(rules: &[PermissionRule]) -> String {
    if rules.is_empty() {
        return "rules: (none)".into();
    }
    let mut lines = vec![format!("rules ({}):", rules.len())];
    for r in rules {
        let effect = match r.effect {
            PermissionEffect::Allow => "allow",
            PermissionEffect::Reject => "reject",
            PermissionEffect::Ask => "ask",
        };
        let content = match &r.content {
            Some(c) => format!(" {c:?}"),
            None => String::new(),
        };
        lines.push(format!("  {} {}{content}", r.action, effect));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests;
