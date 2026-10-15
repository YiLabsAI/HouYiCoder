//! The extraction prompt the forked sub-agent receives as its user message.
//! The forked agent runs on the same system prompt prefix as the main loop
//! (the memory behavior taxonomy is already in that prefix), so this prompt is
//! a short user-message addendum that scopes the extraction task: read the
//! recalled manifest first to avoid duplicates, then save only what is
//! non-obvious and non-derivable, using the structured save-memory tool.
//!
//! Kept in its own module so the system-prompt assembler stays under the
//! file-size gate; the string is byte-stable (no volatile inputs) so a
//! forked run that shares the provider benefits from prefix caching.

#[cfg(test)]
use houyicoder_context::MemoryOrigin;
#[cfg(test)]
use houyicoder_context::MemoryScope;
#[cfg(test)]
use houyicoder_context::MemorySource;
use houyicoder_context::MemorySummary;

/// The extraction user prompt. Assumed to be appended after the main system
/// prompt and the cloned conversation prefix. Instructs the agent to read
/// the appended manifest (when the store is non-empty), then write new
/// memories via the save-memory tool without interleaving reads and
/// writes. Byte-stable across runs.
pub fn extraction_prompt() -> String {
    r#"You are a memory-extraction sub-agent. Your sole job is to read the recent conversation and save memories that will help future sessions on this project, using the save_memory tool. You are not answering the user; you are distilling durable context.

The system prompt already defines the four memory types (user, feedback, project, reference), the what-not-to-save gate, and the body structure for feedback and project memories. Apply them here.

Procedure:
1. A recalled memories manifest is appended below when the store is non-empty. Read it first so you do not save a duplicate of an existing entry. If a fact refines an existing entry, reuse its key to refresh it. If no manifest is appended, the store is empty.
2. The conversation above is the complete evidence window for this pass. Every message in it is eligible; no earlier history is present, so there is nothing to re-read or re-verify. Do not ask for older context — earlier turns were already extracted or are derivable from the code.
3. Decide what is worth saving. A memory must capture context NOT derivable from the current project state: code patterns, architecture, file paths, git history, and fix recipes are derivable — do NOT save them. Save only: user role or preferences you learned; corrective or validating feedback on how to work (with the why); project goals, decisions, or coordination not in git (with the why); pointers to external systems.
4. Save each candidate via the save_memory tool with a kebab-case key, a specific one-line description naming the entities, the correct source type, the body (with Why and How-to-apply lines for feedback and project types), and an evidence array of one to three quotes copied verbatim from the conversation above. Pick short snippets that ground this memory in what was actually said — two or three precise quotes beat one long one. The host validates each quote against the window and rejects a save whose quotes do not appear there before it writes; the manifest below is not evidence, so a save keyed on a fact only the manifest states has no quote to cite and is rejected.
5. You may issue multiple save_memory calls in parallel within one turn. Do not interleave reads with writes — the appended manifest (when present) is the read step; go straight to writing.
6. If nothing in the recent conversation is non-obvious and non-derivable, save nothing. An empty extraction is the correct outcome for a turn with no durable signal — do not force a save.

Be selective. A small set of high-signal memories beats a large set of noise. The what-not-to-save gate applies even when the user explicitly asks to save a derivable summary — distill the surprising or non-obvious part, not the activity log.

User corrections are the strongest feedback signal. Watch for pushback on how you wrote or named something — phrases like "this comment is verbose", "this name is bad", "don't use X", or any correction to your style or wording. Save each as a feedback-type memory with a feedback_* key (one correction per key; if the same correction recurs, refresh the same key), the user's exact wording as the description, and the why + how-to-apply in the body. These are the reward signals that steer future sessions away from the same friction.

Environment assertions (a tool fails in a sandbox, a command is unavailable, a path is denied) are a different signal from user corrections. Write them as dated observations with a falsification step, not imperatives — the environment can change while the claim sits in memory. Bad: "Don't run cargo in the sandbox, it fails." Good: "2026-08-13: cargo failed in the sandbox with openssl.cnf denied; this is a profile allowlist state, not a permanent fact — verify with cargo --version before relying on cargo under the sandbox." The dated observation plus verify instruction lets a later session self-falsify when the profile changes, instead of obeying a stale claim."#
        .to_string()
}

/// Compose the forked extraction prompt with the existing-memory manifest.
/// The conversation prefix the forked agent already saw is the complete
/// evidence window, so the prompt states that boundary once and appends the
/// manifest for dedup.
pub fn build_extraction_prompt(memories: &[MemorySummary]) -> String {
    let mut prompt = extraction_prompt();
    prompt.push_str(
        "\n\n## Eligible conversation window\n\nThe messages above are the complete evidence window. Cite only what they contain, and copy the evidence quotes for each save_memory call verbatim from these messages. The manifest below names memories that already exist and is not itself evidence.",
    );
    if memories.is_empty() {
        return prompt;
    }
    prompt.push_str("\n\n## Existing memory files\n\n");
    for m in memories.iter().take(200) {
        prompt.push_str(&format!(
            "- {} [{}] mtime={}: {}\n",
            m.key,
            m.source.as_label(),
            m.mtime_secs,
            m.description,
        ));
    }
    prompt.push_str(
        "\nCheck this list before writing — update an existing file rather than creating a duplicate.",
    );
    prompt
}

#[cfg(test)]
mod tests;
