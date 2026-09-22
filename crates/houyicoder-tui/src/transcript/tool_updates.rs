//! First pass of frame projection: resolve each tool call's outcome and
//! output from its matching update so the projection can color the call chip
//! and carry the precomputed body without re-reading the update.

use crate::records::ToolOutcome;
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::frontend::session_update::ToolCallStatus;

/// A resolved tool update awaiting pairing with its call: the call id, the
/// outcome the result row colors by, and the raw output the body reads.
pub(super) type PendingUpdate = (String, Option<ToolOutcome>, Option<serde_json::Value>);

/// The title and raw input a ToolCall frame carries, keyed by call id so an
/// orphan result can still name its tool. First-write is fine for orphans.
pub(super) type ToolRegistry =
    std::collections::HashMap<String, (String, Option<serde_json::Value>)>;

/// Resolve each tool call's outcome + output from its matching ToolCallUpdate
/// (by tool_call_id) so the call chip colors by outcome and the result row
/// carries the precomputed body. Also record the tool name + raw_input from
/// the ToolCall so the result row's brief is correct. Tool-call updates are
/// kept in an ordered Vec and consumed FIFO per call_id, not a last-write-wins
/// HashMap. Eager tool callers reuse one call_id across distinct calls; a
/// HashMap would collapse them to the last insert and every result row would
/// show the same body. FIFO consume pairs each call with its own matching
/// update. The tools map stays a HashMap: the call row reads the title + input
/// from the ToolCall frame itself, and tools only names the tool for an
/// orphan result (no call frame in the stream), where first-write is fine.
pub(super) fn collect_tool_updates<F: AsRef<TranscriptFrame>>(
    frames: &[F],
) -> (Vec<PendingUpdate>, ToolRegistry) {
    use houyicoder_protocol::frontend::session_update::SessionUpdate;
    use std::collections::HashMap;
    let mut updates: Vec<PendingUpdate> = Vec::new();
    let mut tools: ToolRegistry = HashMap::new();
    for f in frames {
        match f.as_ref() {
            TranscriptFrame::Session(SessionUpdate::ToolCall(tc)) => {
                tools.insert(
                    tc.tool_call_id.0.clone(),
                    (tc.title.clone(), tc.raw_input.clone()),
                );
            }
            TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(upd)) => {
                push_update(&mut updates, &mut tools, upd);
            }
            _ => {}
        }
    }
    (updates, tools)
}

/// Push one resolved update for a ToolCallUpdate frame, drawing the tool name
/// and call input from the registry the matching ToolCall populated.
fn push_update(
    updates: &mut Vec<PendingUpdate>,
    tools: &mut ToolRegistry,
    upd: &houyicoder_protocol::frontend::session_update::ToolCallUpdate,
) {
    let id = upd.tool_call_id.0.clone();
    let Some(out) = &upd.fields.raw_output else {
        if let Some(status) = upd.fields.status {
            let oc = match status {
                ToolCallStatus::Failed => ToolOutcome::Error,
                ToolCallStatus::Completed => ToolOutcome::Success,
                _ => ToolOutcome::Running,
            };
            updates.push((id, Some(oc), None));
        }
        return;
    };
    // Semantic error judgment needs the tool name + call input (grep/diff exit
    // 1 is not an error). The tools map is populated by the ToolCall frame,
    // which arrives before its update, so the entry is present here.
    let (tool_name, call_input) = tools.get(&id).cloned().unwrap_or_default();
    let outcome = ToolOutcome::from_output_with(
        out,
        &tool_name,
        call_input.as_ref().unwrap_or(&serde_json::Value::Null),
    );
    updates.push((id, Some(outcome), Some(out.clone())));
}
