//! Turn tool-call collapse: consecutive tool calls collapse to one dim
//! summary line, expandable via ctrl+o or click. Render-layer only.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::git_op::{self, GitOp};
use crate::records::{ToolOutcome, TranscriptLine};

/// Per-type counters for one turn's tool calls. Drives the summary line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolStats {
    pub bash: u32,
    pub search: u32,
    /// Distinct file paths read via the Read tool (or a path-bearing bash
    /// read). Dedup'd by path so re-reading the same file counts once —
    /// a read-path set. See read_count.
    pub read_paths: HashSet<String>,
    /// Pathless read operations (e.g. cat, head, tail bash commands
    /// with no file_path to dedup on). Counted by occurrence.
    pub read_ops: u32,
    pub list: u32,
    pub edit: u32,
    pub write: u32,
    pub todo: u32,
    pub other: u32,
    /// Memory store writes via the save_memory tool. Tracked apart from the
    /// edit/write file buckets so the summary reads "wrote N memory" (a
    /// memory-write bucket) instead of "edited N files" — a
    /// memory save is a meta-operation on the agent's own state, not a
    /// source-file edit.
    pub mem_write: u32,
    /// Memory store deletes via the delete_memory tool. Houyi has a
    /// dedicated delete tool; folding it into mem_write
    /// would read "wrote" for a delete, so it gets its own count and the
    /// "deleted N memory" verb. Destructive op, surfaced for visibility.
    pub mem_delete: u32,
}

impl ToolStats {
    /// Total read count: distinct paths read + pathless read operations.
    /// Adding both (not paths.len() if !empty else ops) keeps cat calls
    /// visible alongside file reads — "read 3 files" + "ran 2 cat" must
    /// surface the cats, not drop them when a path read also occurred.
    pub fn read_count(&self) -> u32 {
        self.read_paths.len() as u32 + self.read_ops
    }

    pub fn total(&self) -> u32 {
        self.bash
            + self.search
            + self.read_count()
            + self.list
            + self.edit
            + self.write
            + self.todo
            + self.other
            + self.mem_write
            + self.mem_delete
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// The bucket a bash command falls into, so find . | wc -l reads as a list
/// plus a generic shell step, not two generic commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BashKind {
    Search,
    Read,
    List,
    Bash,
}

const SEARCH_TOKENS: &[&str] = &["grep", "rg", "ag", "ack", "fgrep", "egrep"];
const READ_TOKENS: &[&str] = &["cat", "head", "tail", "less", "more", "bat"];
const LIST_TOKENS: &[&str] = &["ls", "tree", "find", "du", "df", "lsof", "ps"];

/// Classify one bash command into a single bucket by its leading
/// sub-commands (split on &&/;/|, first matching token wins).
pub(crate) fn classify_bash(command: &str) -> BashKind {
    for raw in command.split(['&', ';', '|']) {
        let first = raw.split_whitespace().next().unwrap_or("");
        if first.is_empty() {
            continue;
        }
        if SEARCH_TOKENS.contains(&first) {
            return BashKind::Search;
        }
        if READ_TOKENS.contains(&first) {
            return BashKind::Read;
        }
        if LIST_TOKENS.contains(&first) {
            return BashKind::List;
        }
    }
    BashKind::Bash
}

/// Whether a tool call folds into a turn-summary group. The collapsible
/// set: search/read/list bash commands, grep, glob, read, WebFetch, and
/// memory writes are collapsible; edit, multiedit, write, todo_write, and
/// other tools render individual (each its own message) so their content
/// stays visible by default — these stay as individual messages, not
/// folded into a cross-tool summary.
fn is_foldable(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "bash" | "grep" | "glob" | "read" | "WebFetch" | "save_memory" | "delete_memory"
    )
}

/// Accumulate one tool call into the stats. Tool names follow the registered
/// name; a bash call is sub-classified by its command.
pub(crate) fn accumulate(stats: &mut ToolStats, tool: &str, input: &Value) {
    match tool {
        "bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            // A git-op bash call (commit/push/pr) is surfaced as a git op
            // in the group, not counted as a generic bash command.
            if git_op::is_git_op_command(cmd) {
                return;
            }
            match classify_bash(cmd) {
                BashKind::Search => stats.search += 1,
                // cat/head/tail have no file_path to dedup on: count by
                // occurrence (read_ops), not into read_paths.
                BashKind::Read => stats.read_ops += 1,
                BashKind::List => stats.list += 1,
                BashKind::Bash => stats.bash += 1,
            }
        }
        // Read tool: dedup by file_path so re-reading the same file counts
        // once (a read-path set). A missing
        // file_path (malformed input) falls back to read_ops so the call is
        // still surfaced.
        "read" => {
            if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
                stats.read_paths.insert(p.to_string());
            } else {
                stats.read_ops += 1;
            }
        }
        "grep" => stats.search += 1,
        "glob" => stats.list += 1,
        "edit" | "multiedit" => stats.edit += 1,
        "write" => stats.write += 1,
        "save_memory" => stats.mem_write += 1,
        "delete_memory" => stats.mem_delete += 1,
        "todo_write" => stats.todo += 1,
        _ => stats.other += 1,
    }
}

/// Accumulate one tool call using the transcript's invocation string (the
/// untruncated call-line argument: command / path / pattern) instead of raw
/// input JSON. Receives the RAW tool title (not the chip name) so an Edit
/// call buckets as edit even though its chip reads Update.
pub(crate) fn accumulate_brief(stats: &mut ToolStats, tool: &str, invocation: &str) {
    match tool {
        "bash" => {
            if git_op::is_git_op_command(invocation) {
                return;
            }
            match classify_bash(invocation) {
                BashKind::Search => stats.search += 1,
                BashKind::Read => stats.read_ops += 1,
                BashKind::List => stats.list += 1,
                BashKind::Bash => stats.bash += 1,
            }
        }
        // Read tool: the invocation IS the file_path. Dedup by path.
        "read" => {
            stats.read_paths.insert(invocation.to_string());
        }
        "grep" => stats.search += 1,
        "glob" => stats.list += 1,
        "edit" | "multiedit" => stats.edit += 1,
        "write" => stats.write += 1,
        "WebFetch" => stats.search += 1,
        "save_memory" => stats.mem_write += 1,
        "delete_memory" => stats.mem_delete += 1,
        "todo_write" => stats.todo += 1,
        _ => stats.other += 1,
    }
}

/// One maximal run of consecutive Tool call+result pairs in a turn segment.
/// The summary row replaces the run when collapsed; expanding renders each
/// line. Keyed by call_id#ordinal so same-call_id groups do not collide.
#[derive(Debug, Clone)]
pub(crate) struct FoldGroup {
    /// call_id#ordinal — stable across rebuild and unique per group.
    pub key: String,
    /// Start transcript index (inclusive).
    pub start: usize,
    /// End transcript index (exclusive).
    pub end: usize,
    /// Accumulated per-type counters for the summary line.
    pub stats: ToolStats,
    /// True when the group is in the active (still-streaming) turn.
    pub active: bool,
    /// Preview of the group's LAST foldable call, shown as a ⎿ row under
    /// the collapsed summary so the user sees what's in the group without
    /// expanding. bash → $ <cmd> (or the # label the model wrote), read
    /// → path, grep/glob → "pattern", WebFetch → url. None when the
    /// group has no hint-bearing call.
    pub hint: Option<String>,
    /// Git operations (commit/push/merge/rebase/pr) surfaced from bash
    /// calls in this group. Lead the summary line ("committed abc123,
    /// pushed to main, created PR #42") so the load-bearing outcome reads
    /// first; the bash count excludes these calls (a git commit is not
    /// also "ran 1 shell command").
    pub git_ops: Vec<GitOp>,
}

/// Scan for foldable groups: maximal runs of consecutive Tool call+result
/// pairs. Error calls break runs; diff-bearing calls no longer do (they
/// fold in). A group is active (never collapsed) when it sits in the last
/// turn segment and the agent is busy.
pub(crate) fn compute_fold_groups(
    transcript: &[TranscriptLine],
    agent_busy: bool,
) -> Vec<FoldGroup> {
    let last_turn_start = last_turn_boundary(transcript);
    let mut groups = Vec::new();
    // Per-call_id ordinal so same-call_id groups (eager callers reuse one
    // call_id) get distinct keys (c1#0, c1#1, ...).
    let mut ordinal: HashMap<String, u32> = HashMap::new();
    let mut i = 0;
    while i < transcript.len() {
        // Look for a Tool call (name != "result") to start a group.
        let Some((call_id, name, status)) = tool_call_at(transcript, i) else {
            i += 1;
            continue;
        };
        // Error calls are exempt: skip and do not fold.
        if tool_call_outcome(transcript, i) == Some(ToolOutcome::Error) {
            i += 1;
            // Skip the matching result if present.
            if is_result_for(transcript, i, &call_id) {
                i += 1;
            }
            continue;
        }
        // Non-foldable tools (edit, multiedit, write, todo_write, ...) render
        // individual (each its own call+result) so their content stays
        // visible by default. These stay as individual
        // messages; only the search/read/list bash, grep, glob, read,
        // WebFetch, and memory-write tools fold into a turn summary.
        if !is_foldable(&name) {
            i += 1;
            if is_result_for(transcript, i, &call_id) {
                i += 1;
            }
            continue;
        }
        // Start accumulating a group.
        let start = i;
        let n = ordinal.entry(call_id.clone()).or_insert(0);
        let key = format!("{call_id}#{n}");
        *n += 1;
        let mut stats = ToolStats::default();
        let mut git_ops: Vec<GitOp> = Vec::new();
        accumulate_brief(&mut stats, &name, &status);
        if let Some(op) = detect_gitop_for_call(transcript, start, &call_id, &name, &status) {
            git_ops.push(op);
        }
        // Track the LAST foldable call's (tool, invocation) for the ⎿ hint
        // shown under the collapsed summary — the hint reflects what's most
        // recently happening in the group, not the first call.
        let mut last_call: Option<(String, String)> = Some((name.clone(), status.clone()));
        i += 1; // Consume the matching result.
        if is_result_for(transcript, i, &call_id) {
            i += 1;
        }
        // Extend the group with further consecutive call+result pairs.
        while let Some((cid, nm, st)) = tool_call_at(transcript, i) {
            if tool_call_outcome(transcript, i) == Some(ToolOutcome::Error) {
                break;
            }
            // A non-foldable tool (edit, write, ...) breaks the run so it
            // renders as its own individual call+result (content visible),
            // not buried under a cross-tool summary. These stay
            // individual rather than folding into an aggregate.
            if !is_foldable(&nm) {
                break;
            }
            let call_idx = i;
            accumulate_brief(&mut stats, &nm, &st);
            if let Some(op) = detect_gitop_for_call(transcript, call_idx, &cid, &nm, &st) {
                git_ops.push(op);
            }
            last_call = Some((nm.clone(), st.clone()));
            i += 1;
            if is_result_for(transcript, i, &cid) {
                i += 1;
            }
        }
        let end = i;
        let active = agent_busy && start >= last_turn_start;
        let hint = last_call.and_then(|(t, inv)| compute_hint(&t, &inv));
        if stats.total() > 0 || !git_ops.is_empty() {
            groups.push(FoldGroup {
                key,
                start,
                end,
                stats,
                active,
                hint,
                git_ops,
            });
        }
    }
    groups
}

/// Detect a git op for one bash call: the command (invocation) + the
/// matching result's body (raw stdout+stderr). Returns None for non-bash,
/// non-git-op, or in-flight calls whose result has not landed yet (the
/// git op surfaces once the result arrives).
fn detect_gitop_for_call(
    transcript: &[TranscriptLine],
    call_idx: usize,
    call_id: &str,
    name: &str,
    invocation: &str,
) -> Option<GitOp> {
    if name != "bash" || !git_op::is_git_op_command(invocation) {
        return None;
    }
    let result_idx = call_idx + 1;
    if !is_result_for(transcript, result_idx, call_id) {
        return None;
    }
    let (body, _) = transcript.get(result_idx)?.result_body();
    Some(git_op::detect_git_operation(invocation, &body))
}

/// Cap on the ⎿ $ <command> hint so a 200-line bash heredoc does not dump
/// into the collapsed summary (a max-hint-chars cap).
const HINT_CAP: usize = 300;

/// Extract a # label comment the model wrote for the human (a
/// bash-comment-label extraction). Returns the trimmed label text, or None when the
/// command has no leading comment. Used as the hint for a bash call so the
/// group reads the model's own one-line summary, not a raw command dump.
fn extract_bash_comment_label(command: &str) -> Option<&str> {
    for line in command.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            let label = rest.trim();
            if !label.is_empty() {
                return Some(label);
            }
            continue;
        }
        // First non-comment, non-blank line: no leading comment label.
        return None;
    }
    None
}

/// Truncate a bash command to HINT_CAP chars for the ⎿ hint, preserving
/// newlines so continuation lines indent under ⎿ (the renderer wraps). Drops
/// blank lines and collapses inline whitespace, mirroring commandAsHint.
fn command_hint(command: &str) -> String {
    let cleaned_body: String = command
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let mut cleaned = String::from("$ ");
    cleaned.push_str(&cleaned_body);
    if cleaned.chars().count() > HINT_CAP {
        let mut s: String = cleaned.chars().take(HINT_CAP - 1).collect();
        s.push('…');
        s
    } else {
        cleaned
    }
}

/// Build the ⎿ hint for the group's last foldable call. bash → the # label/// if the model wrote one, else $ <command> (capped); read → the path;
/// grep/glob → "pattern"; WebFetch → the url. None for tools with no
/// meaningful preview.
fn compute_hint(tool: &str, invocation: &str) -> Option<String> {
    if invocation.is_empty() {
        return None;
    }
    match tool {
        "bash" => extract_bash_comment_label(invocation)
            .map(|l| l.to_string())
            .or_else(|| Some(command_hint(invocation))),
        "read" => Some(invocation.to_string()),
        "grep" | "glob" => Some(format!("\"{invocation}\"")),
        "WebFetch" => Some(invocation.to_string()),
        _ => None,
    }
}

/// If transcript[i] is a Tool call (name != "result"), return (call_id,
/// raw tool title, invocation). The raw title drives fold-bucketing so an
/// Edit call buckets as edit, not other. The invocation (untruncated
/// command / path / pattern) drives both bucketing (classify_bash, path
/// dedup) and the ⎿ hint shown under the collapsed summary.
fn tool_call_at(transcript: &[TranscriptLine], i: usize) -> Option<(String, String, String)> {
    let line = transcript.get(i)?;
    match line {
        TranscriptLine::Tool {
            name,
            tool,
            call_id,
            invocation,
            ..
        } if name != "result" => Some((call_id.clone(), tool.clone(), invocation.clone())),
        _ => None,
    }
}

/// The outcome of a Tool call at transcript index i (None if not a Tool call).
fn tool_call_outcome(transcript: &[TranscriptLine], i: usize) -> Option<ToolOutcome> {
    match transcript.get(i)? {
        TranscriptLine::Tool { outcome, .. } => Some(*outcome),
        _ => None,
    }
}

/// True when transcript[i] is a Tool result (name == "result") for the given
/// call_id.
fn is_result_for(transcript: &[TranscriptLine], i: usize, call_id: &str) -> bool {
    match transcript.get(i) {
        Some(TranscriptLine::Tool {
            name, call_id: cid, ..
        }) => name == "result" && cid == call_id,
        _ => false,
    }
}

/// The index immediately after the last User or Agent line — the start of the
/// last turn segment. 0 when no such line exists (the whole transcript is one
/// segment).
fn last_turn_boundary(transcript: &[TranscriptLine]) -> usize {
    let mut last = 0;
    for (i, line) in transcript.iter().enumerate() {
        match line {
            TranscriptLine::User(_) | TranscriptLine::Agent(_) => last = i + 1,
            _ => {}
        }
    }
    last
}

/// Rendered summary construction (per-bucket counts + git ops + the
/// ctrl+o-to-expand affordance) lives in the summary submodule so this
/// file stays under the file-size gate.
mod summary;
pub(crate) use summary::render_summary;

/// One visible block in the transcript flow. Both the count path and the
/// render path walk the same Vec of slots so their row counts never diverge.
#[derive(Debug, Clone)]
pub(crate) enum DisplaySlot {
    /// Render the transcript line at this index in full. The key is
    /// Some(group_key) inside an EXPANDED fold group, None otherwise.
    Line(usize, Option<String>),
    /// A collapsed non-active group's one-row summary (replaces the lines).
    Summary(FoldGroup),
}

/// Build the visible slot list from the transcript, fold groups, and the
/// expanded-set. Collapsed groups produce one Summary slot; expanded groups
/// produce the Summary header (for completed groups) plus individual Line
/// slots. A single-call active group stays expanded so the user sees the
/// in-flight call. Multi-call active groups collapse to a live summary so a
/// long exploration run does not fill the screen with consecutive bash calls.
/// Lines outside any group always produce a Line slot.
pub(crate) fn display_slots(
    transcript: &[TranscriptLine],
    agent_busy: bool,
    expanded: &HashSet<String>,
    verbose: bool,
) -> Vec<DisplaySlot> {
    let groups = compute_fold_groups(transcript, agent_busy);
    let mut slots = Vec::new();
    let mut gi = 0;
    let mut i = 0;
    while i < transcript.len() {
        while gi < groups.len() && groups[gi].end <= i {
            gi += 1;
        }
        if gi < groups.len() && groups[gi].start == i {
            let g = &groups[gi];
            let is_expanded = expanded.contains(&g.key);
            // Active (in-flight) groups show each call directly — the user
            // sees each tool call + its folded result as it lands, not a
            // live summary that hides them (active multi-call groups stay
            // expanded, not buried behind a "Running N commands" line).
            // Completed groups collapse to the summary
            // unless the user expanded one — then the Summary stays as a
            // collapse-handle header above the lines. Verbose mode (the
            // search view) forces every group expanded so a search hit is
            // never hidden behind a collapsed turn summary.
            let should_collapse = !(is_expanded || g.active || verbose);
            if should_collapse {
                slots.push(DisplaySlot::Summary(g.clone()));
            } else {
                // Expanded: the Summary stays as a clickable header (the
                // group's collapse handle) for completed groups, then each
                // line. Active groups show just the live calls.
                if !g.active {
                    slots.push(DisplaySlot::Summary(g.clone()));
                }
                // Tag each expanded-group line with the group key so the view
                // can paint the expanded block + route a clean click anywhere
                // in it to collapse this group (expanded block = one click region).
                for j in i..g.end {
                    slots.push(DisplaySlot::Line(j, Some(g.key.clone())));
                }
            }
            i = g.end;
            gi += 1;
        } else {
            slots.push(DisplaySlot::Line(i, None));
            i += 1;
        }
    }
    slots
}

// Fold-aware display-row counting, split from state.rs so that file stays
// under the file-size gate. These walk the same display_slots Vec the render
// path walks, so the count and the rendered rows never diverge.
impl crate::state::App {
    /// Total display rows the current transcript renders to. A line may span
    /// multiple rows (a tool result body renders its summary, continuations,
    /// and an optional collapse hint), and a blank spacer is inserted before
    /// each top-level message except the first. Folded groups contribute one
    /// summary row instead of their individual lines. All counted so paging
    /// stays aligned with the rendered row space.
    ///
    /// Prefers the value the last render published (an O(1) Cell read) over
    /// recomputing the walk. The draw path builds the rendered rows anyway and
    /// publishes rows.len() to transcript_scroll.total; this is the
    /// count==render single source. Callers that query between renders (scroll
    /// math, the status bar, run_control) read the published value instead of
    /// each re-walking the transcript - the prior design had scroll read the
    /// Cell while the status bar + run_control recomputed, a second source
    /// that drifted a frame + paid the cold-cache O(n x parse) on the draw
    /// path. Falls back to the recompute before the first render publishes.
    pub fn transcript_display_rows(&self) -> usize {
        let t = self.transcript_scroll.total.get();
        if t > 0 { t } else { self.fold_aware_rows(None) }
    }

    /// The display-row index where transcript line idx starts (sum of row
    /// counts of all earlier lines, plus one blank spacer per earlier
    /// message). Used to jump the scroll to a search match. A line inside a
    /// collapsed fold group maps to the group's summary row position.
    pub fn transcript_row_of_line(&self, idx: usize) -> usize {
        self.fold_aware_rows(Some(idx))
    }

    /// Walk the transcript summing display rows. When target is Some(idx),
    /// returns the row where line idx starts. When None, returns the total.
    ///
    /// Fold-aware counting: a completed turn's consecutive tool calls collapse
    /// to one summary row (when not expanded) or expand to individual lines
    /// plus a collapse-hint row (when expanded). The slot region is
    /// single-sourced: both this count path and the render path
    /// (view::working::draw_transcript) walk the same display_slots Vec with
    /// the same spacer logic. The trailing live-preview and spinner rows
    /// (drawn after the slot region while the agent is busy or a reply
    /// streams) are added via live_trailing_row_count, which matches the
    /// render path's post-slot appends so the totals still agree.
    pub(crate) fn fold_aware_rows(&self, target: Option<usize>) -> usize {
        let transcript = self.active_transcript();
        let slots = display_slots(
            transcript,
            self.agent_busy,
            &self.expanded_fold_groups,
            self.verbose,
        );
        let mut total = 0;
        let mut first = true;
        for slot in &slots {
            let (needs_spacer, rows) = match slot {
                DisplaySlot::Line(i, _) => (true, self.line_display_rows(&transcript[*i])),
                DisplaySlot::Summary(g) => (true, 1 + g.hint.is_some() as usize),
            };
            if !first && needs_spacer {
                total += 1;
            }
            if let Some(t) = target {
                let found = match slot {
                    DisplaySlot::Line(i, _) => *i == t,
                    DisplaySlot::Summary(g) => t >= g.start && t < g.end,
                };
                if found {
                    return total;
                }
            }
            total += rows;
            first = false;
        }
        // Trailing live-preview + spinner rows are drawn after the slot region
        // (see view::working::draw_transcript) when the agent is busy or a
        // reply is streaming. The total path must include them so the count
        // matches the rendered total; the target path returns within the loop
        // above (all transcript line indices sit inside the slot region).
        if target.is_none() {
            total += self.live_trailing_row_count(total == 0);
        }
        total
    }
}

#[cfg(test)]
#[path = "fold_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "fold_group_tests.rs"]
mod fold_group_tests;
