//! Supporting record types for the App state: the transcript line, the
//! tool-approval popup, the spec-context strip data, and the status bar
//! values. Extracted from state.rs so state.rs holds only App and the core
//! navigation enums (Screen / Stage / Pane). All fields are stub.

use houyicoder_protocol::frontend::context::ContextBreakdown;

/// Drill-down rows under the /context grid: per-file memory and per-skill
/// footprints. These drill-down rows list in two sections below the grid;
/// the stub path carries canned entries so the layout is faithful before
/// the real analyzer is wired.
#[derive(Debug, Clone, Default)]
pub struct ContextDrillDown {
    pub memory_files: Vec<ContextFileEntry>,
    pub skills: Vec<ContextSkillEntry>,
}

/// One memory-file row: display path + token count.
#[derive(Debug, Clone)]
pub struct ContextFileEntry {
    pub path: String,
    pub tokens: u32,
}

/// One skill row: source group (Built-in / Project / ...) + name + tokens.
#[derive(Debug, Clone)]
pub struct ContextSkillEntry {
    pub source: String,
    pub name: String,
    pub tokens: u32,
}

/// One canned suggestion row under the grid: severity glyph + bold title +
/// dim detail, optionally a savings estimate.
#[derive(Debug, Clone)]
pub struct ContextSuggestion {
    pub severity: SuggestionSeverity,
    pub title: String,
    pub detail: String,
    pub savings_tokens: Option<u32>,
}

/// Suggestion severity maps to the glyph + color: warning (yellow) or info
/// (cyan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionSeverity {
    Warning,
    Info,
}

/// The full /context view payload: the proportional grid + categories, the
/// drill-down rows, and the canned suggestions. Carried by the ContextGrid
/// transcript line so the inline renderer has everything it needs.
#[derive(Debug, Clone, Default)]
pub struct ContextView {
    pub breakdown: ContextBreakdown,
    pub drill: ContextDrillDown,
    pub suggestions: Vec<ContextSuggestion>,
}

/// The Esc-abort notice, gutter included. Dim, one line, welded under the
/// message the abort cut off. The two leading spaces plus the gutter glyph
/// plus two trailing spaces are the same child-row prefix tool results use,
/// so an interrupt reads as an annotation on the message above it rather
/// than as a fresh utterance in the conversation.
pub const INTERRUPTED_NOTICE: &str = "  ⎿  Interrupted · What should Houyi do instead?";

/// One line in the work transcript (action log, not chat bubbles).
#[derive(Debug, Clone)]
pub enum TranscriptLine {
    /// A user task or instruction typed in the input box.
    User(String),
    /// A short agent action summary.
    Agent(String),
    /// A file read action.
    Read { path: String },
    /// A file edit action with a one-line summary.
    Edit { path: String, summary: String },
    /// A tool invocation with a one-word status + the call's outcome (colored
    /// by the matching ToolResult: Running until the result lands, then
    /// Success/Error). For a result row (name == "result") the body and
    /// is_diff are precomputed at transcript-build time from the output JSON
    /// (extract_body + output_has_diff) so the per-frame render does no JSON
    /// parsing (hot path); call_id keys the per-result expand state across
    /// transcript rebuilds.
    Tool {
        /// User-facing chip name (Edit renders as "Update"/
        /// "Create"); built from tool by tool_user_facing_name at transcript
        /// build time so the per-frame render does no remapping.
        name: String,
        /// Raw registered tool title ("edit", "WebFetch", ...) carried
        /// alongside the user-facing name so fold-bucketing (accumulate_brief)
        /// matches on the stable raw title, not the display name ("Update"
        /// would otherwise mis-bucket as "other"). None-equivalent is the
        /// empty string for the synthetic "result" row.
        tool: String,
        status: String,
        /// The untruncated call-line argument (command / path / pattern, or
        /// canonical full JSON for an unknown tool). Precomputed at build time
        /// from the same tool_invocation projection the verbose view and search
        /// read, so index-equals-render is structural: a search hit is always
        /// on text the verbose view shows. The chip uses the truncated status;
        /// the verbose view and search use this verbatim. Empty for the
        /// synthetic result row (a result has no invocation).
        invocation: String,
        outcome: ToolOutcome,
        call_id: String,
        body: String,
        is_diff: bool,
    },
    /// System or slash-command feedback.
    System(String),
    /// The Esc-abort notice. Rendered as a dim child row welded under the
    /// message the abort cut off, not as a top-level notice: the interrupt is
    /// a property of that message (it stopped mid-sentence), not a new event
    /// in the conversation. The interrupt notice renders
    /// through the same child-annotation container as tool results, and this
    /// reuses the gutter tool results already use here for the same reason.
    Interrupted,
    /// Model reasoning (a first-class message type, not folded into System).
    /// NOT rendered as a row above the answer (the reasoning folds into
    /// the ThoughtFor line below the answer, expandable via
    /// Ctrl+O). search_text returns the full text so /search can find it.
    Thinking { text: String },
    /// The per-turn "thought for Ns" line, carrying the turn's reasoning text
    /// so Ctrl+O can expand it inline below the answer (the
    /// "Thought for Ns (ctrl+o to expand)" shape). reasoning is None when the
    /// turn emitted no reasoning (no hint, no expand).
    ThoughtFor {
        secs: u32,
        reasoning: Option<String>,
        /// One-line tool-call summary for this turn ("ran 3 tools (2 bash,
        /// 1 grep)"); None when the turn ran no tools.
        tool_summary: Option<String>,
        /// Stable unique id for THIS turn's ThoughtFor (a session counter
        /// minted at Done). The expand/collapse state (expanded_thinking)
        /// is keyed by this, NOT by the reasoning text — two turns can
        /// produce identical reasoning text, and keying by reasoning would
        /// collide them so expanding one expanded both. Survives transcript
        /// rebuilds because ThoughtFor is TUI-only (preserved, not
        /// re-derived) on rebuild.
        turn_id: String,
    },
    /// /context breakdown rendered INLINE as conversation content (multi-row
    /// block). Carries the full view payload so the renderer lays it out
    /// without re-deriving data on each frame.
    ContextGrid(ContextView),
    /// A sub-agent delegation rendered INLINE as a fold-group in the parent
    /// flow. The parent message list is never swapped out. The agent tool's
    /// structured result carries child_sid + subagent_type + summary. Default
    /// collapsed (summary); Ctrl+O/click expands. child_sid keys the
    /// per-child expand state across transcript rebuilds (the
    /// ThoughtFor.turn_id pattern).
    Subagent {
        child_sid: String,
        subagent_type: String,
        summary: String,
        /// The task prompt the parent handed the child, surfaced so the
        /// teammate banner can name the task this view is for (the result
        /// lives in the transcript body). Empty when the call input lacks
        /// a prompt.
        prompt: String,
        /// The child's transcript projected through the same pipeline.
        /// Empty: the on-expand fetch from the child session log is not
        /// yet wired.
        folded_transcript: Vec<TranscriptLine>,
        /// The agent's badge color, surfaced from the resolved
        /// AgentDefinition so the teammate header and the inline summary
        /// row share one source. None when the agent has no color set.
        color: Option<String>,
    },
}

/// Build an inline Subagent fold-group line from an agent-tool result, or
/// None when the output is not an agent delegation. The agent tool's
/// structured result carries agentId (the child session id) and content (the
/// summary); the call input carries subagent_type + the task prompt. The
/// child transcript is fetched on expand, so folded_transcript starts
/// empty. Detecting by agentId (not the tool title) is robust: only the
/// agent tool emits this field, and the title can be a display name.
pub(crate) fn subagent_line(
    output: &serde_json::Value,
    call_input: Option<&serde_json::Value>,
) -> Option<TranscriptLine> {
    let child_sid = output.get("agentId")?.as_str()?.to_string();
    let summary = output
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let subagent_type = call_input
        .and_then(|v| v.get("subagent_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("general-purpose")
        .to_string();
    let prompt = call_input
        .and_then(|v| v.get("prompt"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let color = output
        .get("color")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some(TranscriptLine::Subagent {
        child_sid,
        subagent_type,
        summary,
        prompt,
        folded_transcript: Vec::new(),
        color,
    })
}

/// A teammate transcript the user drilled into from a Subagent fold-group.
/// While open, the working surface swaps to render the child's projected
/// turns instead of the parent's, with a banner naming the agent. The child
/// transcript is the same projection the inline fold-group fetch fills, so the
/// drilled-in view is isomorphic with the expanded fold, not a simplified
/// list. Opened by Enter on a Subagent line, closed by Esc.
#[derive(Debug, Clone, Default)]
pub struct TeammateView {
    /// The child session id, keying the fetch.
    pub child_sid: String,
    /// The subagent type, shown in the banner.
    pub subagent_type: String,
    /// The task prompt the parent handed the child, shown dim under the
    /// banner title so the banner names the task this view is for.
    pub prompt: String,
    /// The agent's badge color, applied to the banner name. None renders
    /// the default foreground.
    pub color: Option<String>,
    /// The child's projected transcript. Empty until the fetch returns.
    pub transcript: Vec<TranscriptLine>,
}

/// A tool call's resolved outcome, for chip coloring. Running = no matching
/// result yet (in flight); Success/Error = the matching ToolResult's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Running,
    Success,
    Error,
}

impl ToolOutcome {
    /// Derive the outcome from a tool result's output JSON. An error key or
    /// success == false → Error; otherwise Success (the call returned).
    ///
    /// For bash, a non-zero exit is NOT always an error: grep exits 1 when
    /// there are no matches, diff exits 1 when files differ — both are
    /// successful completions of the command's semantic job, not failures.
    /// The command text (from the call's input) is inspected to recognize
    /// these semantic-success cases so they do not render red. Other tools
    /// keep the simple error-key / success==false rule.
    pub fn from_output(output: &serde_json::Value) -> Self {
        Self::from_output_with(output, "", &serde_json::Value::Null)
    }

    /// Same as from_output, but with the tool name + call input so bash
    /// commands whose non-zero exit is semantic success (grep no-match,
    /// diff files-differ) are not mis-colored as errors.
    pub fn from_output_with(
        output: &serde_json::Value,
        tool_name: &str,
        call_input: &serde_json::Value,
    ) -> Self {
        let has_error = output.get("error").is_some();
        let success_false = output.get("success").and_then(|v| v.as_bool()) == Some(false);
        if !has_error && !success_false {
            return Self::Success;
        }
        // bash with a non-zero exit: check whether the command is one whose
        // non-zero exit is semantic success, not a failure.
        if tool_name == "bash" && !has_error {
            let exit_code = output
                .get("exit_code")
                .and_then(|c| c.as_i64())
                .unwrap_or(0);
            let command = call_input
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            if exit_code != 0 && command_is_semantic_success(command, exit_code) {
                return Self::Success;
            }
        }
        Self::Error
    }
}

/// Whether a bash command's non-zero exit is a semantic success (the command
/// did its job), not a failure. grep exits 1 when no matches are found; diff
/// exits 1 when files differ; both are the command reporting a result, not
/// failing. A non-zero exit is not always an error — the exit code is the
/// command's verdict, and some commands use non-zero to mean "I found
/// something" or "the inputs differ", which is the whole point of running
/// them.
fn command_is_semantic_success(command: &str, exit_code: i64) -> bool {
    // Only the common, unambiguous cases. A compound command yields no
    // command word (its exit code belongs to the last stage), so it stays
    // an error on non-zero.
    let Some(word) = crate::bash_command::simple_command_word(command) else {
        return false;
    };
    match (word, exit_code) {
        ("grep", 1) => true, // no matches — the command succeeded
        ("rg", 1) => true,   // ripgrep — same
        ("diff", 1) => true, // files differ — the command succeeded
        _ => false,
    }
}

impl TranscriptLine {
    /// True when this line is TUI-only (not derived from session events).
    /// Slash-command User echoes (starting with /), ContextGrid, System,
    /// and Approval lines have no matching TurnEvent, so the transcript
    /// rebuild must preserve them at their original positions instead of
    /// appending them at the end.
    pub fn is_tui_only(&self) -> bool {
        match self {
            Self::ContextGrid(_)
            | Self::System(_)
            | Self::ThoughtFor { .. }
            | Self::Interrupted => true,
            Self::User(s) => s.starts_with('/'),
            _ => false,
        }
    }

    /// Render the line as a glyph-led string for the transcript pane. No
    /// role pipe-prefix: a leading glyph carries the role, matching the
    /// clean chat surface. > user, ● assistant, ✻ system notice,
    /// ⎿ tool/continuation.
    pub fn render(&self) -> String {
        self.render_with(false)
    }

    /// Verbose render: tool-call chips use the untruncated invocation (not
    /// the truncated status) so a search hit on a long command lands on the
    /// text the verbose view actually shows (index-equals-render). Other
    /// line kinds render the same as the folded form — the verbose view
    /// expands their bodies via the per-item expand state, not here.
    pub fn render_verbose(&self) -> String {
        self.render_with(true)
    }

    fn render_with(&self, verbose: bool) -> String {
        match self {
            Self::User(s) => format!("> {s}"),
            Self::Agent(s) => format!("● {s}"),
            Self::Read { path } => format!("  ⎿  read {path}"),
            Self::Edit { path, summary } => format!("  ⎿  edit {path} — {summary}"),
            Self::Tool {
                name,
                status,
                invocation,
                body,
                ..
            } => {
                if name == "result" {
                    let first = body.split('\n').next().unwrap_or("").to_string();
                    format!("  ⎿  {first}")
                } else {
                    let arg = if verbose { invocation } else { status };
                    let display = capitalize(name);
                    format!("⏺ {display}({arg})")
                }
            }
            Self::System(s) => format!("✻ {s}"),
            Self::Interrupted => INTERRUPTED_NOTICE.to_string(),
            Self::Thinking { text } => format!("✻ {}", thinking_brief(text)),
            Self::ThoughtFor {
                secs,
                reasoning,
                tool_summary,
                ..
            } => match (reasoning, tool_summary) {
                (Some(_), Some(ts)) => {
                    format!("✻ Thought for {}s, {} (ctrl+o to expand)", secs, ts)
                }
                (Some(_), None) => {
                    format!("✻ Thought for {}s (ctrl+o to expand)", secs)
                }
                (None, Some(ts)) => format!("✻ Thought for {}s, {}", secs, ts),
                (None, None) => format!("✻ Thought for {}s", secs),
            },
            // The grid block is rendered by the working-surface renderer as a
            // multi-row inline widget (grid + legend side-by-side, drill-down,
            // suggestions). render() returns a flat-text fallback so /search
            // and any non-grid path still surface the legend text.
            Self::ContextGrid(view) => context_grid_text(view),
            // A delegation surfaces its summary as the searchable text; the
            // child transcript is fetched on expand, not inlined here.
            Self::Subagent { summary, .. } => summary.clone(),
        }
    }

    /// The multi-line body for a tool result row, plus whether it is a diff
    /// body (Edit/MultiEdit) so the renderer can color +/− lines. Both are
    /// precomputed at build time (hot path: no per-frame JSON parse). The body
    /// is a one-line summary followed by the payload (a diff for edits, stdout
    /// for Bash, file content for Read, a wrote-line for Write). Empty for
    /// non-result lines. The working-surface renderer lays this out with a
    /// leading ⎿ gutter on the first line, a continuation indent on the rest,
    /// and collapse + Ctrl+O expand for long bodies; the summary carries
    /// Added/removed counts (see brief::edit_diff_summary)
    /// counts for edits.
    pub fn result_body(&self) -> (String, bool) {
        match self {
            Self::Tool {
                name,
                body,
                is_diff,
                ..
            } if name == "result" => (body.clone(), *is_diff),
            _ => (String::new(), false),
        }
    }

    /// The text to search when the user runs /search (the popup, not the
    /// inline Ctrl+F which scans the visible rows). A tool result's full body
    /// (summary + diff / stdout / content) is searched, not just the summary
    /// line render() returns — so a match inside a diff body is found and
    /// jumpable. Other lines use render().
    pub fn search_text(&self) -> String {
        match self {
            Self::Tool { name, body, .. } if name == "result" => body.clone(),
            Self::Thinking { text } => text.clone(),
            Self::ThoughtFor {
                reasoning: Some(r), ..
            } => r.clone(),
            _ => self.render(),
        }
    }
}

/// Collapse reasoning to one display line: the first line plus a +N hint when
/// the reasoning spans multiple lines, so a long chain-of-thought does not
/// flood the transcript. The full text stays in the variant for search/expand.
pub fn thinking_brief(text: &str) -> String {
    // Collapsed marker: the reasoning content is NOT shown by default (a
    // collapsed thinking block the user expands, not the
    // streaming content). The full text stays on the TranscriptLine for
    // /search and a future Ctrl+O expand. Only a line-count hint surfaces.
    let lines: usize = text.split('\n').filter(|l| !l.trim().is_empty()).count();
    if lines > 1 {
        format!("thinking (+{lines} lines)")
    } else {
        "thinking".to_string()
    }
}

/// Capitalize the first letter of a tool name for display (bash → Bash).
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Flat-text fallback for the /context grid block. The working-surface
/// renderer draws the real multi-row grid+legend widget; this string exists
/// so /search and any path that flattens the line still surfaces the legend
/// text (model, categories, suggestions) for matching.
fn context_grid_text(view: &ContextView) -> String {
    let bd = &view.breakdown;
    let pct = if bd.context_window > 0 {
        100.0 * bd.total_tokens as f64 / bd.context_window as f64
    } else {
        0.0
    };
    let mut s = String::from("\u{23BF} Context Usage\n");
    s.push_str(&format!(
        "  {} \u{00b7} {}/{} tokens ({:.0}%)\n",
        bd.model, bd.total_tokens, bd.context_window, pct,
    ));
    s.push_str("  Estimated usage by category\n");
    for cat in &bd.categories {
        if cat.tokens == 0 || cat.is_deferred || cat.is_reserved || cat.label == "Free space" {
            continue;
        }
        let c_pct = if bd.context_window > 0 {
            100.0 * cat.tokens as f64 / bd.context_window as f64
        } else {
            0.0
        };
        s.push_str(&format!(
            "  {}: {} tokens ({:.1}%)\n",
            cat.label, cat.tokens, c_pct
        ));
    }
    if !view.drill.memory_files.is_empty() {
        s.push_str("  Memory files\n");
        for f in &view.drill.memory_files {
            s.push_str(&format!("  \u{2514} {}: {} tokens\n", f.path, f.tokens));
        }
    }
    if !view.drill.skills.is_empty() {
        s.push_str("  Skills\n");
        for k in &view.drill.skills {
            s.push_str(&format!("  \u{2514} {}: {} tokens\n", k.name, k.tokens));
        }
    }
    if !view.suggestions.is_empty() {
        s.push_str("  Suggestions\n");
        for sug in &view.suggestions {
            s.push_str(&format!(
                "  {} {}\n    {}\n",
                sug.severity.glyph(),
                sug.title,
                sug.detail
            ));
        }
    }
    s.trim_end().to_string()
}

impl SuggestionSeverity {
    /// Glyph prefix for a suggestion row: warning -> the heavy warning mark,
    /// info -> the circled i.
    pub fn glyph(self) -> char {
        match self {
            Self::Warning => '\u{26A0}',
            Self::Info => '\u{2139}',
        }
    }
}

/// Tool-approval prompt state. Three options: approve, reject, or
/// always-allow (approve + add a permission rule so the same tool no
/// longer prompts). call_id links the prompt back to the
/// ApprovalRequest from the agent loop so resume() gets the right decision.
#[derive(Debug, Clone, Default)]
pub struct Approval {
    /// Tool name awaiting approval.
    pub tool: String,
    /// Serialized arguments (placeholder).
    pub args: String,
    /// Why the tool wants to run: the structured AskReason detail the gate
    /// produced, or a generic prompt when the composition root could not
    /// reconstruct one.
    pub reason: String,
    /// The source of the ask, when the wire carried a reason. Drives the
    /// card: a SystemSafety source hides the "remember" option because
    /// consent cannot override a protected-path check.
    pub source: Option<houyicoder_protocol::frontend::permission::AskSource>,
    /// An optional note from the containment layer the card renders as a
    /// fourth line when present.
    pub containment_note: Option<String>,
    /// The index of the currently highlighted option (0-based). The initial
    /// cursor is chosen by priority: a sticky last-used choice for this tool
    /// (matched by identity, not list position) wins; otherwise YOLO when the
    /// permission mode auto-approves (Auto / Bypass) focuses the quickest
    /// approve; otherwise index-0. raise_agent_approval computes it.
    pub selected: usize,
    /// The agent-loop call_id this popup resolves. Empty for the legacy stub
    /// flow; filled from ApprovalRequest when the real runner raises an
    /// Interruption so resume() can match the decision.
    pub call_id: String,
    /// The verdict options the server offered (N-option). Empty when the
    /// server deferred; the card falls back to its built-in 3-option set.
    pub options: Vec<houyicoder_protocol::acp_wire::PermissionOption>,
}

/// The number of visible options in the approval prompt.
pub const APPROVAL_OPTIONS: usize = 3;

impl Approval {
    /// Whether the "Yes, and don't ask again" option is hidden. A protected-
    /// path check (source SystemSafety) is authoritative: a stored consent
    /// rule cannot override it, so persisting a yes-always would be a no-op
    /// at best and misleading at worst. Hide the option so the human is not
    /// offered a choice the gate will ignore.
    pub fn remember_hidden(&self) -> bool {
        matches!(
            self.source,
            Some(houyicoder_protocol::frontend::permission::AskSource::SystemSafety)
        )
    }

    /// The count of visible options: two (Yes / No) when remember is hidden,
    /// otherwise the built-in three.
    pub fn visible_option_count(&self) -> usize {
        if self.remember_hidden() {
            2
        } else {
            crate::records::APPROVAL_OPTIONS
        }
    }

    /// Label for the currently focused button. Aligned to the
    /// yes / no / yes-dont-ask-again option names (the card renders these).
    pub fn focus_label(&self) -> &'static str {
        match self.selected {
            0 => "yes",
            1 => "no",
            _ => "yes-dont-ask-again",
        }
    }

    /// True when the focused option approves the tool call (Yes or
    /// Yes-don't-ask-again both send an approve decision to the runner).
    pub fn focused_approves(&self) -> bool {
        self.selected != 1
    }

    /// True when the focused option is Yes-don't-ask-again (approve + persist
    /// a session-scoped permission rule for this tool).
    pub fn focused_persists(&self) -> bool {
        self.selected == 2
    }

    /// The identity of the focused option for the built-in 3-option set: the
    /// kind the choice resolves to (Yes → allow-once, No → reject-once,
    /// Yes-don't-ask → allow-always). The cursor-priority preselect matches
    /// by this identity rather than by list position, so a sticky choice
    /// carries across even when the offered list order changes.
    pub fn focused_kind(&self) -> houyicoder_protocol::acp_wire::PermissionOptionKind {
        match self.selected {
            0 => houyicoder_protocol::acp_wire::PermissionOptionKind::AllowOnce,
            1 => houyicoder_protocol::acp_wire::PermissionOptionKind::RejectOnce,
            _ => houyicoder_protocol::acp_wire::PermissionOptionKind::AllowAlways,
        }
    }

    /// The built-in 3-option index whose kind matches. Reject-always has no
    /// slot in the built-in set, so it collapses to the reject-once index (a
    /// reject is a reject for cursor-preselect purposes). An unknown kind
    /// lands on reject too — the preselect never auto-focuses an approve it
    /// cannot identify.
    pub fn index_for_kind(kind: houyicoder_protocol::acp_wire::PermissionOptionKind) -> usize {
        use houyicoder_protocol::acp_wire::PermissionOptionKind as K;
        match kind {
            K::AllowOnce => 0,
            K::AllowAlways => 2,
            K::RejectOnce | K::RejectAlways => 1,
        }
    }
}

pub use crate::ask_question_model::{AskQuestion, OTHER_LABEL, QuestionCard, QuestionOption};
/// Spec context strip data: the current spec, step, and clause list.
#[derive(Debug, Clone)]
pub struct SpecContext {
    pub spec_id: String,
    pub title: String,
    pub step: String,
    /// The currently focused clause index in App.spec_clauses.
    pub clause_focus: usize,
}

/// Status bar values. All stub.
#[derive(Debug, Clone)]
pub struct StatusStub {
    pub session_id: String,
    pub model: String,
    pub path: String,
    pub tokens: u64,
    pub capability: String,
    pub sandbox: String,
    pub plan_mode: bool,
    /// Whether the most-recent run ended FinalOutput (a clean end). Read by
    /// idle_drain to gate the queued-message auto-send: a clean end auto-sends
    /// the next queued item (the user got their answer, drain FIFO); an
    /// interrupt/error does NOT auto-send — the queued item stays parked for
    /// the user to pop to the input box via Esc + edit before re-sending.
    /// A redirect on interrupt should not auto-fire the pending input.
    pub last_run_final: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transcript_render_glyphs() {
        assert!(TranscriptLine::User("x".into()).render().starts_with(">"));
        assert!(TranscriptLine::Agent("hi".into()).render().starts_with("●"));
        assert!(
            TranscriptLine::System("note".into())
                .render()
                .starts_with("✻")
        );
        assert!(
            TranscriptLine::Read { path: "p".into() }
                .render()
                .contains("read")
        );
    }

    // I2 (call-chip slice): verbose render uses the untruncated invocation
    // while folded render uses the truncated status. A search hit on a long
    // command lands on the text the verbose view shows — index-equals-render
    // for the call chip.
    #[test]
    fn test_verbose_render_uses_invocation() {
        let long = "x".repeat(300);
        let call = TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: "x".repeat(160),  // truncated chip form
            invocation: long.clone(), // untruncated
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            body: String::new(),
            is_diff: false,
        };
        let folded = call.render();
        let verbose = call.render_verbose();
        // Folded chip carries the truncated status, not the 300-char tail.
        assert!(folded.contains(&"x".repeat(160)));
        assert!(!folded.contains(&long));
        // Verbose chip carries the full invocation.
        assert!(verbose.contains(&long));
    }

    #[test]
    fn test_result_body_only_result() {
        let call = TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: "ls".into(),
            invocation: "ls".into(),
            outcome: ToolOutcome::Running,
            call_id: "c1".into(),
            body: String::new(),
            is_diff: false,
        };
        assert_eq!(call.result_body(), (String::new(), false));
        let result = TranscriptLine::Tool {
            name: "result".into(),
            tool: "bash".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            body: "ok".into(),
            is_diff: false,
        };
        assert_eq!(result.result_body(), ("ok".to_string(), false));
        // render() of a result shows the first body line under the gutter.
        assert_eq!(result.render(), "  ⎿  ok");
    }

    #[test]
    fn test_thinking_collapses_multiline() {
        let line = TranscriptLine::Thinking {
            text: "first line\nsecond\nthird".into(),
        };
        let r = line.render();
        // Collapsed marker only (no content); the full text stays for search.
        assert!(r.contains("thinking"), "got {r}");
        assert!(r.contains("+3 lines"), "should hint 3 lines, got {r}");
        assert!(!r.contains("first line"), "content must not show, got {r}");
        // search_text returns the full text (not the collapsed render).
        assert_eq!(line.search_text(), "first line\nsecond\nthird");
    }

    #[test]
    fn test_thinking_line_no_plus() {
        let line = TranscriptLine::Thinking {
            text: "only line".into(),
        };
        let r = line.render();
        // Collapsed marker only (no content); no +N hint for a single line.
        assert!(r.contains("thinking"));
        assert!(!r.contains("only line"), "content must not show, got {r}");
        assert!(!r.contains("+"));
    }
}

#[cfg(test)]
#[path = "records_tests.rs"]
mod records_tests;
