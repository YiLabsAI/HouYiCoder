//! Supporting record types for the App state: the transcript line, the
//! tool-approval popup, the spec-context strip data, and the status bar
//! values. Extracted from state.rs so state.rs holds only App and the core
//! navigation enums (Screen / Stage / Pane).

use houyicoder_protocol::block_ref::PREVIEW_KEY;
use houyicoder_protocol::frontend::context::ContextBreakdown;

mod approval;
pub use approval::{APPROVAL_OPTIONS, Approval};

mod subagent;
pub use subagent::{SubagentFinished, SubagentOutcome};

use crate::row_id::RowId;
use crate::scroll::TranscriptScroll;
use crate::toggle_hint::ToggleHint;
use crate::transcript::shell_result::ShellStorage;
use crate::view::context_view::context_grid_text;
use crate::view::relative_time::turn_span_label;

/// Drill-down rows under the /context grid: per-source memory and per-skill
/// footprints. A memory row is a project memory file, the memory index, or the
/// recalled memories. These rows list in two sections below the grid; the
/// measurement supplies them, so the renderer never makes up rows from a pane
/// cache.
#[derive(Debug, Clone, Default)]
pub struct ContextDrillDown {
    pub memory_files: Vec<ContextFileEntry>,
    pub skills: Vec<ContextSkillEntry>,
}

impl ContextDrillDown {
    /// Read the drill-down rows the breakdown carries. The measurement produces
    /// them, so the frontend renders exactly what the breakdown reported.
    pub fn from_breakdown(breakdown: &ContextBreakdown) -> Self {
        Self {
            memory_files: breakdown
                .memory_files
                .iter()
                .map(|f| ContextFileEntry {
                    label: f.label.clone(),
                    tokens: f.tokens,
                })
                .collect(),
            skills: breakdown
                .skills
                .iter()
                .map(|s| ContextSkillEntry {
                    source: s.source.clone(),
                    name: s.name.clone(),
                    tokens: s.tokens,
                    announced: s.announced,
                })
                .collect(),
        }
    }
}

/// One memory-source row: a project memory file name, the memory index, or the
/// recalled memories, with the tokens it costs.
#[derive(Debug, Clone)]
pub struct ContextFileEntry {
    pub label: String,
    pub tokens: u32,
}

/// One skill row: discovery origin key + name + tokens, with whether the
/// listing announces the skill. A skill the listing does not announce costs
/// the window nothing and shows without a token figure.
#[derive(Debug, Clone)]
pub struct ContextSkillEntry {
    pub source: String,
    pub name: String,
    pub tokens: u32,
    pub announced: bool,
}

/// One suggestion row under the grid: severity glyph + bold title + dim
/// detail, optionally a savings estimate.
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
/// drill-down rows, and the suggestions. Carried by the ContextGrid transcript
/// line so the inline renderer has everything it needs.
///
/// The measured breakdown is boxed: it is a few hundred bytes of wire DTO, and
/// every transcript row would otherwise pay for it inside the row enum, while
/// the view is read by reference on each render.
#[derive(Debug, Clone, Default)]
pub struct ContextView {
    pub breakdown: Box<ContextBreakdown>,
    pub drill: ContextDrillDown,
    pub suggestions: Vec<ContextSuggestion>,
}

/// The Esc-abort notice, gutter included. Dim, one line, welded under the
/// message the abort cut off. The two leading spaces plus the gutter glyph
/// plus two trailing spaces are the same child-row prefix tool results use,
/// so an interrupt reads as an annotation on the message above it rather
/// than as a fresh utterance in the conversation.
pub const INTERRUPTED_NOTICE: &str = "  ⎿  Interrupted · What should Houyi do instead?";

/// The row standing in for the part of a turn the resident frames no longer
/// hold. A window opening inside a turn whose opening frame was drained would
/// begin mid-turn with nothing saying why; the earlier part is in the session
/// log, so the row sends the reader there.
pub const TURN_CONTENT_OUT_OF_VIEW: &str =
    "earlier content of this turn is out of view — scroll up to load";

/// The marker leading a recap row. The line's own text and its wrap path both
/// read it here, so a wrapped summary hangs under the words it continues
/// instead of under the marker's second column.
pub const RECAP_MARKER: &str = "※ recap: ";

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
        /// empty string for the result row.
        tool: String,
        status: String,
        /// The untruncated call-line argument (command / path / pattern, or
        /// canonical full JSON for an unknown tool). Precomputed at build time
        /// from the same tool_invocation extraction the verbose view and
        /// search read, so index-equals-render is structural: a search hit is
        /// always on text the verbose view shows. The chip uses the truncated
        /// status; the verbose view and search use this verbatim. Empty for
        /// the result row (a result has no invocation).
        invocation: String,
        outcome: ToolOutcome,
        call_id: String,
        body: String,
        is_diff: bool,
    },
    /// A command the user typed at the dollar prompt. It renders as a prompt
    /// row holding the command and, on the right, what the host measured;
    /// its output is the result row below it, which expands through the same
    /// path a model-issued shell call uses. The row never folds into a turn's
    /// tool summary: the user ran the command, so it stays where it happened.
    ShellCommand {
        /// The command text, as typed.
        command: String,
        outcome: ToolOutcome,
        /// The host's measured wall clock, in ms. None when it did not time
        /// the call, a fact kept distinct from a measured zero.
        duration_ms: Option<u64>,
        /// The command's exit status from the result. None while the call
        /// runs, and when the call never ran.
        exit_code: Option<i64>,
        /// The command hit its time limit. A timeout is not a failure the
        /// command returned, so it reads yellow rather than red, even though
        /// the result is an error.
        timed_out: bool,
        /// What the host kept of the raw output beyond the bounded text.
        /// None when the result named no capture.
        storage: Option<ShellStorage>,
        /// The one-line label for an output moved to the block store, when
        /// the result carried such a marker.
        cas: Option<String>,
    },
    /// System or slash-command feedback.
    System(String),
    /// A summary of the session the user left: the work in progress and the
    /// next step, written while they were away. It describes the conversation
    /// without being part of it — nothing reads it back to the model, and it
    /// claims no work that the log does not state. Rendered dim at the
    /// transcript tail under a marker that keeps it from reading as a turn.
    Recap(String),
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
    /// The per-turn "thought for" line, carrying the turn's reasoning text
    /// so Ctrl+O can expand it inline below the answer (the
    /// "Thought for 2s (ctrl+o to expand)" shape). reasoning is None when the
    /// turn emitted no reasoning (no hint, no expand). The frame fold
    /// derives this row from the turn's own frames, so the live turn and the
    /// same turn replayed from the log render the same row.
    ThoughtFor {
        /// How long the turn's drive legs ran, summed: the time the agent
        /// spent working, not the wall clock the turn was open for. A wait for
        /// an approval between legs is excluded, so a wait for the user does
        /// not read as thinking time. None when the log carries no completion
        /// record for that turn and the row reads as a bare "Thought"; a
        /// measured span always prints its number, however short.
        ms: Option<u64>,
        reasoning: Option<String>,
        /// One-line tool-call summary for this turn ("ran 3 bash" for one
        /// kind of tool, "ran 3 tools (2 bash, 1 grep)" for mixed kinds);
        /// None when the turn ran no tools.
        tool_summary: Option<String>,
        /// Identity of THIS turn's row: the log entry that ended the turn when
        /// the frames carry one, else the absolute frame position that ended
        /// it. The same entry names the same row in every read, live or from
        /// disk, and across a session reload. The expand/collapse state for a
        /// thinking block is keyed by this, NOT by the reasoning text: two
        /// turns can produce identical text, and keying by reasoning would
        /// collide them so expanding one expanded both.
        turn_id: RowId,
    },
    /// /context breakdown rendered INLINE as conversation content (multi-row
    /// block). Carries the full view payload so the renderer lays it out
    /// without re-deriving data on each frame.
    ContextGrid(ContextView),
    /// A context compaction, rendered as one foldable row instead of the two
    /// raw lines the host notification used to contribute.
    ///
    /// The checkpoint and the summary arrive as separate notifications and
    /// both used to land as plain system rows: the checkpoint said nothing a
    /// reader needed, and the summary pasted the model's whole digest into the
    /// transcript, filling the screen with text the reader did not ask for and
    /// cannot act on. One row names the event and hides the body behind the
    /// shared fold chord, so the fact stays visible and the bytes stay on
    /// demand.
    Compaction {
        /// The checkpoint this compaction recorded, used as its fold identity.
        /// Two compactions in one session are two separate blocks; a shared
        /// constant key would expand and collapse both together.
        key: String,
        /// The statistics the checkpoint reported, already formatted for the
        /// row ("754 turns, 5885 events"). Empty when the boundary carried
        /// none, and the row then shows no parenthesis rather than empty ones.
        stats: String,
        /// The summary body, shown only when the row is expanded.
        summary: String,
    },
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
        /// The child's transcript folded through the same pipeline.
        /// Empty: the on-expand fetch from the child session log is not
        /// yet wired.
        folded_transcript: Vec<TranscriptLine>,
        /// The agent's badge color, surfaced from the resolved
        /// AgentDefinition so the teammate header and the inline summary
        /// row share one source. None when the agent has no color set.
        color: Option<String>,
    },
    /// The terminal moment of a child agent, raised in the parent timeline
    /// when its final status arrives. The view of a running child closes on
    /// exit; this row is what lets the reader go back to the delegation after
    /// it ended.
    SubagentFinished(SubagentFinished),
}

/// The one-line label a compaction row shows. The statistics sit in
/// parentheses only when the boundary reported any, so a checkpoint without
/// them reads as the bare event rather than as an empty pair of brackets. The
/// fold chord rides inside the same parentheses, joined by a middle dot, which
/// is the shape every other foldable block in the transcript uses.
///
/// A row with no fold group passes None: it is still named, but it must not
/// advertise a chord that has nothing to toggle. A digest that arrives with no
/// boundary is the one case, and the row is the only trace of that event.
pub(crate) fn compaction_row_text(stats: &str, hint: Option<ToggleHint>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !stats.is_empty() {
        parts.push(stats);
    }
    if let Some(hint) = hint {
        // suffix() is the whole parenthesised affordance for callers that
        // concatenate it after a label. Here the list owns the parentheses, so
        // only the words go in.
        parts.push(hint.action());
    }
    if parts.is_empty() {
        return "\u{273b} Context compacted".to_string();
    }
    format!("\u{273b} Context compacted ({})", parts.join(" \u{00b7} "))
}

/// Build an inline Subagent fold-group line from an agent-tool result, or
/// None when the output is not an agent delegation. The agent tool's
/// structured result carries agentId (the child session id) and content (the
/// summary); the call input carries subagent_type + the task prompt. The
/// child transcript is fetched on expand, so folded_transcript starts
/// empty. Detecting by agentId (not the tool title) is robust: only the
/// agent tool emits this field, and the title can be a display name.
pub(crate) fn subagent_line(output: &Value, call_input: Option<&Value>) -> Option<TranscriptLine> {
    let child_sid = output.get("agentId")?.as_str()?.to_string();
    // The summary is the content string when inline. When the content was
    // field-level externalized (a large child summary), content is the
    // {block_ref, preview, hint} marker object — fall back to the inline
    // preview so the fold-group still shows what the child returned. The
    // agentId stays at the top level either way, so the fold-group renders.
    // The summary is a one-line preview: newlines flatten to spaces, and
    // the text truncates to fit a terminal row (~80 chars) with an ellipsis.
    // The full multiline content shows on Ctrl+O expand.
    let summary = match output.get("content") {
        Some(Value::String(s)) => fold_summary(s),
        Some(c) if c.get(PREVIEW_KEY).is_some() => {
            fold_summary(c.get(PREVIEW_KEY).and_then(|p| p.as_str()).unwrap_or(""))
        }
        _ => String::new(),
    };
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
/// While open, the working view swaps to render the child's folded
/// turns instead of the parent's, with a banner naming the agent. The child
/// transcript is the same fold the inline fold-group fetch fills, so the
/// drilled-in view is isomorphic with the expanded fold, not a simplified
/// list. Opened by Enter on a Subagent line, closed by Shift+Up/Down; Esc
/// interrupts the viewed child's turn.
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
    /// The child's folded transcript. Empty until the fetch returns.
    pub transcript: Vec<TranscriptLine>,
    /// The child viewport's scroll state, owned here so the parent's scroll
    /// keeps its position across the visit. Entering starts it at the tail;
    /// leaving the view destroys it with the view.
    pub scroll: TranscriptScroll,
    /// The turn number of the last live re-fetch, so a Progress for a turn
    /// already fetched does not refire the query. Reset on each enter.
    pub last_fetched_turn: Option<u32>,
    /// A steering message echoed optimistically before the child drains it.
    /// The refetch handler preserves it until the durable User line appears,
    /// then clears it. None when no echo is pending.
    pub pending_echo: Option<String>,
}

/// A tool call's resolved outcome, for chip coloring. Running = no matching
/// result yet; Success/Error = the matching ToolResult's verdict; Refused =
/// a decision kept the call from running (the user declined the approval
/// card, a hook denied it); NotRun = the call never ran for a reason other
/// than a refusal (the run ended, the tool is gone, the edit it followed did
/// not complete), so it carries no verdict either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Running,
    Success,
    Error,
    Refused,
    NotRun,
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
    pub fn from_output(output: &Value) -> Self {
        Self::from_output_with(output, "", &Value::Null)
    }

    /// Same as from_output, but with the tool name + call input so bash
    /// commands whose non-zero exit is semantic success (grep no-match,
    /// diff files-differ) are not mis-colored as errors.
    ///
    /// A result that says its call never ran carries no verdict on an
    /// execution, so it is NotRun rather than either verdict. A count of what
    /// ran must leave it out, and the summary line would otherwise report an
    /// interrupted command as run.
    ///
    /// A refusal is that same absence with a reason behind it, so it is read
    /// first: a refused call is also a call that never ran, and the chip
    /// should say someone decided against it rather than that it merely
    /// stopped.
    pub fn from_output_with(output: &Value, tool_name: &str, call_input: &Value) -> Self {
        // One failure rule, shared through the tool protocol: the trajectory
        // header counts failures by the same definition, so a chip cannot
        // disagree with the header about one result.
        if tool_result_refused(output) {
            Self::Refused
        } else if tool_result_not_run(output) {
            Self::NotRun
        } else if tool_result_failed(output, tool_name, call_input) {
            Self::Error
        } else {
            Self::Success
        }
    }
}

impl TranscriptLine {
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

    /// Whether a blank spacer separates this line from the one above it. The
    /// interrupt notice and a tool result are children of the line above, so
    /// they open no block of their own; every renderer and every row-count
    /// path that mirrors one reads this single answer.
    pub(crate) fn opens_block(&self) -> bool {
        match self {
            Self::Interrupted => false,
            Self::Tool { name, .. } => name != "result",
            _ => true,
        }
    }

    /// The thought-row label for a given expand affordance, built here so the
    /// flat render, the search text, and the row emitter agree on the index.
    /// hint is the toggle the caller offers, None when nothing can expand or
    /// an enclosing block owns it. None for any other line.
    pub(crate) fn thought_row_text(&self, hint: Option<ToggleHint>) -> Option<String> {
        let Self::ThoughtFor {
            ms, tool_summary, ..
        } = self
        else {
            return None;
        };
        let mut text = match ms {
            None => "Thought".to_string(),
            Some(ms) => format!("Thought for {}", turn_span_label(*ms)),
        };
        if let Some(summary) = tool_summary {
            text.push_str(", ");
            text.push_str(summary);
        }
        if let Some(action) = hint {
            text.push_str(action.suffix());
        }
        Some(format!("✻ {text}"))
    }

    /// The rows a recap notice draws, or None for a line that is not one. The
    /// marker leads the first row and every continuation hangs under the
    /// summary text, so a summary longer than the pane reads as one sentence
    /// rather than as a stack of repeated markers.
    pub(crate) fn recap_rows(&self, width: u16) -> Option<Vec<String>> {
        let Self::Recap(text) = self else {
            return None;
        };
        Some(wrap_plain_block(text, RECAP_MARKER, width, None))
    }

    pub(crate) fn tool_call_rows(&self, width: u16, full: bool) -> Option<Vec<String>> {
        let Self::Tool {
            name,
            status,
            invocation,
            ..
        } = self
        else {
            return None;
        };
        if name == "result" {
            return None;
        }
        let prefix = format!("\u{23fa} {}(", capitalize(name));
        if full {
            if width == 0 {
                return Some(
                    format!("{prefix}{invocation})")
                        .split('\n')
                        .map(str::to_string)
                        .collect(),
                );
            }
            return Some(wrap_plain_block(
                &format!("{invocation})"),
                &prefix,
                width,
                None,
            ));
        }
        let rendered = format!("{prefix}{status})");
        if width == 0 {
            return Some(vec![rendered]);
        }
        let max_width = width as usize;
        let fixed = UnicodeWidthStr::width(prefix.as_str()) + 1;
        let row = if UnicodeWidthStr::width(rendered.as_str()) <= max_width {
            rendered
        } else if max_width > fixed {
            format!("{prefix}{})", truncate_width(status, max_width - fixed))
        } else {
            truncate_width(&rendered, max_width)
        };
        Some(vec![row])
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
            Self::ShellCommand { command, .. } => {
                let flat = command.lines().map(str::trim).collect::<Vec<_>>().join(" ");
                format!("$ {flat}")
            }
            Self::System(s) => format!("✻ {s}"),
            Self::Recap(s) => format!("{RECAP_MARKER}{s}"),
            Self::Interrupted => INTERRUPTED_NOTICE.to_string(),
            Self::Thinking { text } => format!("✻ {}", thinking_brief(text)),
            Self::ThoughtFor { .. } => {
                let hint = matches!(
                    self,
                    Self::ThoughtFor {
                        reasoning: Some(_),
                        ..
                    }
                )
                .then_some(ToggleHint::Expand);
                self.thought_row_text(hint).unwrap_or_default()
            }
            // The grid block is rendered by the working-surface renderer as a
            // multi-row inline widget (grid + legend side-by-side, drill-down,
            // suggestions). render() returns a flat-text fallback so /search
            // and any non-grid path still surface the legend text.
            Self::ContextGrid(view) => context_grid_text(view),
            // A delegation surfaces its summary as the searchable text; the
            // child transcript is fetched on expand, not inlined here.
            Self::Subagent { summary, .. } => summary.clone(),
            // The trace reads the same way here as on its own row, so a
            // search for the ending finds it and the row it lands on agrees.
            Self::SubagentFinished(child) => child.text(),
            Self::Compaction { stats, .. } => {
                // The search fallback carries no affordance: nothing on this
                // path toggles anything, so only the event and its
                // statistics belong in the searchable text.
                compaction_row_text(stats, None)
            }
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

    /// The edited file's extension; None when the call named no file.
    pub fn diff_lang(&self) -> Option<&str> {
        match self {
            Self::Tool {
                name, invocation, ..
            } if name == "result" && !invocation.is_empty() => Some(invocation.as_str()),
            _ => None,
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
pub(crate) fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
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

pub use crate::ask_question_model::{AskQuestion, OTHER_LABEL, QuestionCard, QuestionOption};
use crate::brief::fold_summary;
use crate::view::line_wrap::truncate_width;
use crate::view::line_wrap::wrap_plain_block;
use houyicoder_protocol::tool::tool_result_failed;
use houyicoder_protocol::tool::tool_result_not_run;
use houyicoder_protocol::tool::tool_result_refused;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;
/// Spec context strip data: the current spec, step, and clause list.
#[derive(Debug, Clone)]
pub struct SpecContext {
    pub spec_id: String,
    pub title: String,
    pub step: String,
    /// The currently focused clause index in App.spec_clauses.
    pub clause_focus: usize,
}

#[cfg(test)]
mod tests;
