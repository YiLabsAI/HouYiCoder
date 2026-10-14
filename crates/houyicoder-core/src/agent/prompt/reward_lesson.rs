//! The reward-dream prompt: a focused lesson-extraction pass triggered by
//! blind retries, distinct from the consolidation dream's 4-phase prompt.
//!
//! The consolidation prompt (build_consolidation_prompt in auto_dream.rs)
//! is a general-purpose memory maintenance pass: orient, consolidate, prune,
//! flow scope. It ends with "if nothing changed, say so" — a blanket exit
//! that lets the dream skip writing. When the reward gate fires on a blind
//! retry, that exit is wrong: the agent DID retry a known-failed call, so
//! there IS something to learn. This prompt replaces the consolidation
//! structure with a single focused task — extract a lesson from the failure
//! pattern — and removes the blanket exit. The actionable-difference filter
//! stays, but per-pattern (skip a lesson that duplicates an existing one),
//! not as a blanket "nothing changed" escape.
//!
//! Turn budget is tight (REWARD_DREAM_MAX_TURNS = 8): the prompt must be
//! achievable in a few tool calls (show_memory to check, save_memory to
//! write one or two lessons).

use houyicoder_context::MemorySummary;

use crate::agent::auto_dream::INDEX_FILE;
use crate::agent::auto_dream::format_listing;
#[cfg(test)]
use crate::agent::reward_snapshot::format_reward;
use crate::agent::reward_snapshot::{RewardSnapshot, format_reward_data};

/// Build the reward-dream prompt. Unlike the consolidation prompt, this is a
/// single-task extraction: for each blind-retry failure pattern in the
/// reward signal, write or update a lesson_ memory entry. No consolidation,
/// pruning, or scope flow — those are the full-dream's job.
///
/// The reward signal is required (the gate already checked
/// retry_after_error against the threshold), so the caller passes a
/// reference, not an Option. The memory listing + index are included so
/// the dream can check whether a similar lesson already exists before
/// writing (the actionable-difference filter).
pub(crate) fn build_reward_lesson_prompt(
    memory_root: &str,
    listing: &[MemorySummary],
    index_text: &str,
    reward: &RewardSnapshot,
) -> String {
    let reward_data = format_reward_data(reward);
    format!(
        "# Reward Dream: Lesson Extraction\n\n\
        You are performing a reward dream — a focused pass that extracts \
        lessons from blind retries and failure patterns so the next session \
        does not repeat them.\n\n\
        Memory directory: {memory_root}\n\n\
        {reward_data}\n\
        ## Current memories ({count})\n\n{listing}\n\
        ## Current index\n\n{index_text}\n\n\
        ---\n\n\
        ## Task\n\n\
        The agent retried a call that already failed with the same input — a \
        blind retry. For each failure pattern in the reward signal above:\n\n\
        1. Call show_memory with the lesson_ key you would use, to check \
        whether a similar lesson already exists.\n\
        2. If an existing lesson covers the same actionable difference, skip \
        it. A lesson with no actionable difference from what is already in \
        the index is not worth saving — this is a per-pattern filter, not a \
        blanket exit. Do NOT skip every pattern with a generic \"already \
        covered\" unless you can name the existing lesson that covers each.\n\
        3. If the lesson is new or needs updating, call save_memory with a \
        lesson_ key. The body must state:\n\
           - What the failure pattern was (tool, error, retry count).\n\
           - What to do differently next time — a concrete action, not \
        \"be careful\" or \"check first\".\n\
           - When the lesson applies — the trigger condition the agent \
        should recognize before acting.\n\
        4. If a recalled memory is contradicted by the failures above (the \
        agent followed its guidance and still failed), fix or delete it.\n\n\
        Do NOT consolidate, prune, or reorganize existing memories — this is \
        a focused lesson-extraction pass, not a full consolidation dream. Do \
        NOT edit {INDEX_FILE} directly; it regenerates from the topic files \
        after the dream.\n\n\
        Return a brief summary: which lessons you wrote or updated, and which \
        patterns you skipped because an existing lesson already covers them.",
        count = listing.len(),
        listing = format_listing(listing),
        reward_data = reward_data,
        index_text = index_text,
        INDEX_FILE = INDEX_FILE,
    )
}

#[cfg(test)]
mod tests;
