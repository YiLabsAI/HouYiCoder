//! Bounding the user shell history one model request carries.
//!
//! Each command typed at the input box appends a call and its result to the
//! session log, so a long session holds hundreds. Carried in full, the
//! request would grow without limit. The planner keeps the newest commands
//! that fit the turn's budget, folds the rest out of the request, and says so
//! once in one bounded line; the log still holds every command and its
//! output, so the fold costs the request size, not the record.

use std::collections::HashMap;

use houyicoder_context::SessionEvent;
use houyicoder_context::SessionLogEntry;
use houyicoder_protocol::tool::is_user_shell_call_id;
use serde_json::Value;

/// The most command previews the fold notice lists, however many commands it
/// stands for.
const NOTICE_PREVIEWS: usize = 5;

/// The longest one command keeps in a preview, in characters.
const NOTICE_PREVIEW_CHARS: usize = 48;

/// What one kept command costs beyond its call input and result text: the
/// JSON framing of the call message and the result message around them.
const COMMAND_FRAMING_TOKENS: u32 = 48;

/// Tokens held back while the kept commands are chosen, so the notice that
/// describes the folded ones always has room. Whatever the notice does not
/// use stays unspent: the hold only keeps the allocation from ending flush
/// against the budget with no room to say what it dropped.
const NOTICE_RESERVE_TOKENS: u32 = 320;

/// What a request keeps of the user shell history, and what it says about the
/// rest.
#[derive(Debug, Default)]
pub(crate) struct ShellHistoryPlan {
    /// Call ids whose call and result stay in the request, each mapped to the
    /// tokens its logged result term costs. Every other user shell call is
    /// dropped together with its result, so the assistant message that
    /// carried it is never left awaiting an answer.
    kept: HashMap<String, u32>,
    /// The one line that stands in for the commands left out, if any.
    pub(crate) notice: Option<String>,
    /// Tokens the request commits for this history, charged against the
    /// turn's budget before any block retrieval is decided.
    pub(crate) charged_tokens: u32,
}

impl ShellHistoryPlan {
    /// Whether the call stays in the request. A call that is not a user shell
    /// call is never asked about here; the caller filters by id first.
    pub(crate) fn keeps(&self, call_id: &str) -> bool {
        self.kept.contains_key(call_id)
    }

    /// The tokens the plan charges for the logged result of a kept command,
    /// or None when the command is not kept. A retrieval of the result's
    /// block replaces that logged form, and the budget check reads this term
    /// so the two forms never both count.
    pub(crate) fn charged_result_tokens(&self, call_id: &str) -> Option<u64> {
        self.kept.get(call_id).map(|tokens| u64::from(*tokens))
    }

    /// How many commands stay in the request. Reported for diagnostics, so a
    /// request whose history folded more than expected can be traced.
    pub(crate) fn kept_count(&self) -> usize {
        self.kept.len()
    }
}

/// One user shell command in the log, with the call input that carried it and
/// the result that answers it.
struct ShellCommandRecord<'a> {
    call_id: &'a str,
    command: &'a str,
    input: &'a Value,
    output: Option<&'a Value>,
}

/// Choose the user shell commands a request keeps, and the one line that
/// describes the rest.
///
/// The newest commands are kept while their estimated cost fits the budget,
/// and the first one that does not fit folds every older command with it: a
/// task refers to recent work, so a newer command is the better witness, and a
/// gap in the middle of the kept set would read as history that never
/// happened. Nothing here is fetched from the block store: a result is sized
/// as the log holds it, and the result term is reported alongside the kept
/// ids, so a retrieval the retention path later admits replaces that term
/// instead of paying for the same result a second time.
pub(crate) fn plan_shell_history(
    events: &[SessionLogEntry],
    budget_tokens: u32,
    count: &dyn Fn(&str) -> usize,
) -> ShellHistoryPlan {
    let commands = collect_commands(events);
    if commands.is_empty() {
        return ShellHistoryPlan::default();
    }
    let mut plan = ShellHistoryPlan::default();
    let mut spent = 0u32;
    let command_budget = budget_tokens.saturating_sub(NOTICE_RESERVE_TOKENS);
    // Walk newest first so a result's cost is only computed for a command the
    // request would keep; the first one that does not fit marks where folding
    // starts, and every older one is counted without being sized.
    let mut keep_from = 0usize;
    for (index, record) in commands.iter().enumerate().rev() {
        let result_tokens = logged_result_tokens(record);
        let cost = command_cost(record, result_tokens, count);
        if spent.saturating_add(cost) > command_budget {
            keep_from = index + 1;
            break;
        }
        spent += cost;
        plan.kept.insert(record.call_id.to_string(), result_tokens);
    }
    let folded = &commands[..keep_from];
    if folded.is_empty() {
        plan.charged_tokens = spent;
        return plan;
    }
    let remaining = budget_tokens.saturating_sub(spent);
    let (notice, notice_tokens) = folded_notice(folded, remaining, count);
    plan.charged_tokens = spent.saturating_add(notice_tokens);
    plan.notice = Some(notice);
    plan
}

/// Every user shell command in log order, each with the result that answers
/// it when the log holds one.
fn collect_commands(events: &[SessionLogEntry]) -> Vec<ShellCommandRecord<'_>> {
    let mut outputs: HashMap<&str, &Value> = HashMap::new();
    for entry in events {
        if let SessionEvent::ToolResult {
            call_id, output, ..
        } = &entry.event
        {
            outputs.entry(call_id.as_str()).or_insert(output);
        }
    }
    let mut commands = Vec::new();
    for entry in events {
        if let SessionEvent::ToolCall { call_id, input, .. } = &entry.event
            && is_user_shell_call_id(call_id)
        {
            commands.push(ShellCommandRecord {
                call_id,
                command: input.get("command").and_then(Value::as_str).unwrap_or(""),
                input,
                output: outputs.get(call_id.as_str()).copied(),
            });
        }
    }
    commands
}

/// The upper-bound cost of keeping one command: its call input, its logged
/// result term, and the framing around both.
///
/// The request carries the call input serialized, not the bare command text,
/// so a command holding quotes or backslashes costs its escaped length; sizing
/// the unescaped text would undercount exactly the commands that inflate a
/// request most. The result term is the serialized form the log holds, not the
/// block size a marker names: that size is spent only when the retention path
/// retrieves the block, and the logged term is dropped at that moment.
fn command_cost(
    record: &ShellCommandRecord<'_>,
    result_tokens: u32,
    count: &dyn Fn(&str) -> usize,
) -> u32 {
    let serialized =
        serde_json::to_string(record.input).unwrap_or_else(|_| record.command.to_string());
    let call = count(&serialized);
    let total = call
        .saturating_add(result_tokens as usize)
        .saturating_add(COMMAND_FRAMING_TOKENS as usize);
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// The tokens the logged form of a command's result costs: the serialized
/// value as the log holds it, counted in bytes. The byte count is an upper
/// bound on the tokens the serialized value costs, so a request whose parts
/// are sized this way cannot exceed the budget it was planned against.
fn logged_result_tokens(record: &ShellCommandRecord<'_>) -> u32 {
    let bytes = record.output.map_or(0, logged_result_bytes);
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

/// The size of a result as the log holds it, in bytes.
fn logged_result_bytes(output: &Value) -> usize {
    serde_json::to_vec(output).map_or(usize::MAX, |bytes| bytes.len())
}

/// The line that stands in for the folded commands, with the tokens it costs.
///
/// The notice fits the budget the kept commands left: the full form lists a
/// few recent commands, a short form names the count alone, and a request with
/// no room for either still carries the fixed absent line. Silence would be
/// the one reading the design rules out: the model would see a history that
/// stops without saying it was folded, and would answer as if it had seen
/// everything.
fn folded_notice(
    folded: &[ShellCommandRecord<'_>],
    remaining: u32,
    count: &dyn Fn(&str) -> usize,
) -> (String, u32) {
    let full = full_notice(folded);
    let full_tokens = count(&full) as u32;
    if full_tokens <= remaining {
        return (full, full_tokens);
    }
    let pointer = format!(
        "[{} earlier user shell commands are not in this request. \
         The session log still holds them and their output.]",
        folded.len()
    );
    let pointer_tokens = count(&pointer) as u32;
    if pointer_tokens <= remaining {
        return (pointer, pointer_tokens);
    }
    (ABSENT_NOTICE.to_string(), count(ABSENT_NOTICE) as u32)
}

/// The notice shape that names a few recent commands.
fn full_notice(folded: &[ShellCommandRecord<'_>]) -> String {
    let previews: Vec<String> = folded
        .iter()
        .rev()
        .take(NOTICE_PREVIEWS)
        .map(|record| command_preview(record.command))
        .collect();
    format!(
        "[{} earlier user shell commands are not in this request. Most recent: {}. \
         The session log still holds every command and its output.]",
        folded.len(),
        previews.join(" | ")
    )
}

/// The line carried when the budget cannot hold any description of the folded
/// commands. It names no count, so its size does not grow with the history it
/// stands for.
const ABSENT_NOTICE: &str =
    "[The history of user shell commands is not available in this request.]";

/// One command as a single short line: its first line, its runs of whitespace
/// flattened, cut to the preview length.
fn command_preview(command: &str) -> String {
    let first = command.lines().next().unwrap_or("");
    let flat = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= NOTICE_PREVIEW_CHARS {
        return flat;
    }
    let mut out: String = flat.chars().take(NOTICE_PREVIEW_CHARS).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests;
