//! What the session froze, and what the prefix digested.
//!
//! The system prompt has two halves that live outside the conversation: the
//! project memory the cwd walk-up found (plus the date line), and the tool
//! schemas. Both are frozen for the session and digested per assembly, so a
//! cache break can name which half moved instead of reporting an unknown.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;

use houyicoder_protocol::llm::ToolDef;

use super::super::ContextBuilder;
use super::FrozenPrompt;

/// A digest of the two halves of the cached prefix. Equality means the bytes
/// the provider saw last turn are the bytes it will see now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptHash {
    pub(crate) system: u64,
    pub(crate) tools: u64,
}

/// A stable digest of one prefix half. Not a security hash: it only has to
/// change when the bytes change, so a cache break can name the half that
/// moved.
fn digest(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl ContextBuilder {
    /// The session's frozen prompt inputs, read once and held until an
    /// explicit boundary.
    ///
    /// The project memory and the date line live outside the conversation, so
    /// re-reading them every turn lets a worktree switch or midnight rewrite
    /// the prompt prefix and invalidate the whole cached context. They are
    /// frozen here and refreshed only by reset_frozen_prompt.
    pub(crate) fn frozen_prompt_of(&self, cwd: &Path) -> FrozenPrompt {
        let Ok(mut slot) = self.frozen_prompt.lock() else {
            return FrozenPrompt::read(cwd);
        };
        slot.get_or_insert_with(|| FrozenPrompt::read(cwd)).clone()
    }

    /// Release the frozen prompt inputs so the next assembly re-reads the
    /// project memory and the date. Called at a boundary the user chose, where
    /// the cache is being rebuilt anyway.
    pub fn reset_frozen_prompt(&self) {
        if let Ok(mut slot) = self.frozen_prompt.lock() {
            *slot = None;
        }
    }

    /// Digest the prefix this assembly produced, so a later cache break can
    /// name which half of it moved.
    pub(crate) fn record_prompt_hash(&self, system: &str, tool_defs: &[ToolDef]) {
        let hash = PromptHash {
            system: digest(system),
            tools: tool_defs.iter().fold(0u64, |acc, td| {
                acc.wrapping_mul(31)
                    .wrapping_add(digest(&serde_json::to_string(td).unwrap_or_default()))
            }),
        };
        if let Ok(mut slot) = self.prompt_hash.lock() {
            *slot = Some(hash);
        }
    }

    /// The prefix the last assembly produced, or None before the first one.
    pub(crate) fn prompt_hash(&self) -> Option<PromptHash> {
        self.prompt_hash.lock().ok().and_then(|g| *g)
    }
}
