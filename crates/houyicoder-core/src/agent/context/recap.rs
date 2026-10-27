//! Bounded conversation window for a recap request.
//!
//! At most RECAP_MESSAGE_LIMIT messages and RECAP_INPUT_TOKENS input tokens,
//! less what the request spends outside this window, which is its fixed
//! instruction. The window opens on a message that closes every tool group
//! before it and after the last context clear. Control text the runner
//! writes is dropped, and an externalized tool output arrives as its stored
//! marker: the recap never loads stored content.

use std::collections::HashMap;
use std::collections::HashSet;

use houyicoder_context::SessionEvent;
use houyicoder_context::SessionLogEntry;
use houyicoder_protocol::llm::InputItem;

use crate::agent::context::Tokenizer;
use crate::agent::context::turn_group::assemble_model_input;
use crate::agent::context::turn_group::user_text_of;

/// The most messages one recap request may carry.
pub const RECAP_MESSAGE_LIMIT: usize = 30;

/// The most input tokens one recap request may carry, its fixed instruction
/// included.
pub const RECAP_INPUT_TOKENS: u32 = 4096;

/// Why a recap has no conversation to carry. Every cause is a different fact
/// for diagnostics: an empty session needs work, an oversized newest message
/// needs a shorter request, and an exhausted reservation is a configuration
/// problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecapWindowSkip {
    /// The session holds no message after its last context clear.
    Empty,
    /// The request's reservation leaves no room for a message.
    ReserveExhausted,
    /// Even one message exceeds a limit, so any window would misdescribe the
    /// newest work it is supposed to describe.
    NewestTooLarge,
}

/// Build the bounded conversation for a recap from a replayed log.
///
/// Reserve tokens counts what the request spends outside this window, so the
/// caller keeps the whole input under the ceiling instead of assuming the
/// conversation is the only thing sent. The result is the longest message
/// suffix within both limits; a newest message that does not fit on its own is
/// reported rather than truncated, since a partial copy of the latest work
/// would misrepresent it and a recap is optional.
pub fn build_recap_input(
    events: &[SessionLogEntry],
    tokenizer: &Tokenizer,
    reserve_tokens: u32,
) -> Result<Vec<InputItem>, RecapWindowSkip> {
    let budget = u64::from(RECAP_INPUT_TOKENS.saturating_sub(reserve_tokens));
    if budget == 0 {
        return Err(RecapWindowSkip::ReserveExhausted);
    }
    let epoch = events
        .iter()
        .rposition(|entry| matches!(entry.event, SessionEvent::ContextCleared { .. }))
        .map_or(0, |index| index + 1);
    // Control text goes before anything else is decided, so grouping, tool
    // pairing, and assembly all describe the same conversation.
    let conversation: Vec<&SessionLogEntry> = events[epoch..]
        .iter()
        .filter(|entry| !is_control(&entry.event))
        .collect();
    let starts = window_starts(&conversation);
    let Some(newest) = starts.len().checked_sub(1) else {
        return Err(RecapWindowSkip::Empty);
    };
    // A window holding more than the message limit cannot fit, so the search
    // never looks further back than that and its cost follows the sent window,
    // not the session length.
    let first = newest.saturating_sub(RECAP_MESSAGE_LIMIT);
    let tail_start = starts[first];
    let tail: Vec<SessionLogEntry> = conversation[tail_start..]
        .iter()
        .map(|entry| (*entry).clone())
        .collect();
    let candidates: Vec<usize> = starts[first..]
        .iter()
        .map(|start| start - tail_start)
        .collect();

    let fits = |start: usize| -> Option<Vec<InputItem>> {
        let items = assemble_model_input(&tail[start..], None);
        if items.is_empty() || items.len() > RECAP_MESSAGE_LIMIT {
            return None;
        }
        let tokens: u64 = items
            .iter()
            .map(|item| u64::from(tokenizer.count_input(item)))
            .sum();
        (tokens <= budget).then_some(items)
    };

    // A window that opens earlier carries every event a later one carries, so
    // it never has fewer messages or fewer tokens. Fitting therefore holds for
    // a suffix of the candidate starts, and the earliest fitting one is the
    // longest window within both limits.
    let newest_candidate = candidates.len() - 1;
    fits(candidates[newest_candidate]).ok_or(RecapWindowSkip::NewestTooLarge)?;
    let (mut low, mut high) = (0, newest_candidate);
    while low < high {
        let middle = (low + high) / 2;
        if fits(candidates[middle]).is_some() {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    fits(candidates[low]).ok_or(RecapWindowSkip::NewestTooLarge)
}

/// Text the runner injects for its own control flow. The user did not write
/// it, and a summary of the user's work has no reason to read it.
fn is_control(event: &SessionEvent) -> bool {
    matches!(event, SessionEvent::MetaUser { .. })
}

/// Where the conversation may be cut, ascending: the first message of a user
/// run, or an assistant message, and only where no tool result in the
/// remaining window answers a call before it. A call left unanswered by the
/// window costs nothing; a result whose call the window dropped would arrive
/// without its call.
fn window_starts(conversation: &[&SessionLogEntry]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut calls: HashSet<&str> = HashSet::new();
    let mut results: HashMap<&str, usize> = HashMap::new();
    let mut unmatched = 0usize;
    for (index, entry) in conversation.iter().enumerate().rev() {
        let entry = *entry;
        match &entry.event {
            SessionEvent::ToolResult { call_id, .. } => {
                if !calls.contains(call_id.as_str()) {
                    unmatched += 1;
                }
                *results.entry(call_id.as_str()).or_default() += 1;
            }
            SessionEvent::ToolCall { call_id, .. } => {
                if !calls.contains(call_id.as_str())
                    && let Some(count) = results.get(call_id.as_str())
                {
                    unmatched -= count;
                }
                calls.insert(call_id.as_str());
            }
            _ => {}
        }
        if unmatched == 0 && opens_message(conversation, index) {
            starts.push(index);
        }
    }
    starts.reverse();
    starts
}

/// Whether the event at this index begins a message the window may open on.
fn opens_message(conversation: &[&SessionLogEntry], index: usize) -> bool {
    let event = &conversation[index].event;
    if matches!(event, SessionEvent::AssistantMessage { .. }) {
        return true;
    }
    if user_text_of(event).is_none() {
        return false;
    }
    // Consecutive user-role events form one message, so only its first event
    // opens the window.
    index == 0 || user_text_of(&conversation[index - 1].event).is_none()
}

#[cfg(test)]
mod tests;
