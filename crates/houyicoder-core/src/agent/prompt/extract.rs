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
2. Consider only the recent trailing conversation (roughly the last several turns). Do not re-verify or re-read earlier history — assume earlier turns were already extracted or are derivable from the code.
3. Decide what is worth saving. A memory must capture context NOT derivable from the current project state: code patterns, architecture, file paths, git history, and fix recipes are derivable — do NOT save them. Save only: user role or preferences you learned; corrective or validating feedback on how to work (with the why); project goals, decisions, or coordination not in git (with the why); pointers to external systems.
4. Save each candidate via the save_memory tool with a kebab-case key, a specific one-line description naming the entities, the correct source type, and the body (with Why and How-to-apply lines for feedback and project types).
5. You may issue multiple save_memory calls in parallel within one turn. Do not interleave reads with writes — the appended manifest (when present) is the read step; go straight to writing.
6. If nothing in the recent conversation is non-obvious and non-derivable, save nothing. An empty extraction is the correct outcome for a turn with no durable signal — do not force a save.

Be selective. A small set of high-signal memories beats a large set of noise. The what-not-to-save gate applies even when the user explicitly asks to save a derivable summary — distill the surprising or non-obvious part, not the activity log.

User corrections are the strongest feedback signal. Watch for pushback on how you wrote or named something — phrases like "this comment is verbose", "this name is bad", "don't use X", or any correction to your style or wording. Save each as a feedback-type memory with a feedback_* key (one correction per key; if the same correction recurs, refresh the same key), the user's exact wording as the description, and the why + how-to-apply in the body. These are the reward signals that steer future sessions away from the same friction.

Environment assertions (a tool fails in a sandbox, a command is unavailable, a path is denied) are a different signal from user corrections. Write them as dated observations with a falsification step, not imperatives — the environment can change while the claim sits in memory. Bad: "Don't run cargo in the sandbox, it fails." Good: "2026-08-13: cargo failed in the sandbox with openssl.cnf denied; this is a profile allowlist state, not a permanent fact — verify with cargo --version before relying on cargo under the sandbox." The dated observation plus verify instruction lets a later session self-falsify when the profile changes, instead of obeying a stale claim."#
        .to_string()
}

/// Compose the forked extraction user prompt with the existing-memory
/// manifest appended (a formatMemoryManifest pre-inject: the forked agent
/// reads what already exists so it dedups by reusing a key instead of
/// re-saving the same fact each turn). Returns the
/// bare extraction_prompt unchanged when the store is empty, so a fresh
/// store pays no manifest bytes and the prompt stays cache-stable. The
/// manifest rides the final user turn (not the cached system prefix), so
/// appending it never breaks prompt caching across forked runs.
pub fn build_extraction_prompt(memories: &[MemorySummary]) -> String {
    let mut prompt = extraction_prompt();
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
mod tests {
    use super::*;

    /// The extraction prompt must name the save-memory tool and the
    /// what-not-to-save gate — the two load-bearing guardrails. Byte-stable
    /// so the prefix caches across forked runs.
    #[test]
    fn test_extraction_prompt_names_guardrails() {
        let p = extraction_prompt();
        assert!(
            p.contains("save_memory"),
            "prompt must name the structured write tool"
        );
        assert!(
            p.contains("what-not-to-save"),
            "prompt must reference the gate defined in the system prefix"
        );
        assert!(
            p.to_lowercase().contains("do not"),
            "prompt carries explicit do-not-save guidance"
        );
        assert!(
            p.contains("manifest"),
            "prompt tells the agent to read the preinjected manifest"
        );
        assert!(
            p.contains("parallel"),
            "prompt permits parallel writes within a turn"
        );
        assert!(
            p.contains("User corrections"),
            "prompt tells the agent to watch for user corrections as feedback signal"
        );
        assert!(
            p.contains("feedback_*"),
            "prompt names the feedback_* key namespace for corrections"
        );
        assert!(
            p.contains("dated observations"),
            "prompt teaches dated-observation prose for environment assertions"
        );
        assert!(
            p.contains("self-falsify"),
            "prompt teaches environment assertions to carry a falsification step"
        );
    }

    /// Byte-stability: two calls return identical bytes, so a forked run
    /// sharing the provider sees a cache-stable prefix.
    #[test]
    fn test_extraction_prompt_byte_stable() {
        assert_eq!(
            extraction_prompt().as_bytes(),
            extraction_prompt().as_bytes()
        );
    }

    /// An empty store appends no manifest: the composed prompt equals the bare
    /// extraction prompt byte-for-byte (no lie about a manifest that is not
    /// there, no extra bytes on a fresh store).
    #[test]
    fn test_build_prompt_no_manifest() {
        assert_eq!(
            build_extraction_prompt(&[]).as_bytes(),
            extraction_prompt().as_bytes(),
            "empty store must not append a manifest block"
        );
    }

    /// A non-empty store appends the manifest heading + each entry's key so
    /// the forked agent can dedup. The key and the "Existing memory files"
    /// heading are the load-bearing bits the agent reads before writing.
    #[test]
    fn test_build_prompt_appends_manifest() {
        let mem = MemorySummary {
            key: "build-gate".into(),
            description: "make check must stay green".into(),
            source: houyicoder_context::MemorySource::Project,
            mtime_secs: 0,
            scope: houyicoder_context::MemoryScope::Auto,
            origin: houyicoder_context::MemoryOrigin::Unknown,
        };
        let p = build_extraction_prompt(&[mem]);
        assert!(p.contains("## Existing memory files"));
        assert!(p.contains("build-gate"));
        assert!(p.contains("update an existing file rather than creating a duplicate"));
    }
}
