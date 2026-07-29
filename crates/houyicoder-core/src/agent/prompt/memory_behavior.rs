//! Memory behavior prompt: the always-on guidance that tells the agent
//! WHAT to save, WHEN to save it, and how to treat a recalled memory. This is
//! the write-loop brain — without it the agent only saves on an explicit
//! "remember" signal and never proactively accumulates memory.
//!
//! Memory-type taxonomy, adapted: the four content types line up with the
//! MemorySource enum (user, feedback, project, reference); the private/team
//! scope language is replaced by this project's storage scopes (user for
//! cross-session identity, project for shared project conventions);
//! self-references are neutralized. The
//! eval-validated guidance structure (when_to_save / how_to_use /
//! body_structure / what-not-to-save / trusting-recall) is preserved verbatim
//! in intent because that is where the value is.
//!
//! Kept in its own module so the system-prompt assembler stays under the
//! file-size gate; the section is byte-stable across turns (no volatile
//! inputs) so the prefix caches cleanly.

/// The memory-behavior section appended to the system prompt. Assembled once
/// (no inputs) so it is byte-stable across turns unless this source changes.
pub fn memory_behavior_section() -> String {
    r#"# Memory
Memories capture context NOT derivable from the current project state. Code patterns, architecture, git history, and file structure are derivable (via grep, git, or the project memory file) and must NOT be saved as memories.

## Types of memory

There are several discrete types of memory you can store. Each type below declares when to save it and how to use it.

<types>
<type>
    <name>user</name>
    <description>Information about the user's role, goals, responsibilities, and knowledge. Good user memories let you tailor future behavior to the user's preferences and perspective. The aim is to be helpful to this specific user. Avoid writing memories that could be read as a negative judgement or that are irrelevant to the work you are doing together.</description>
    <when_to_save>When you learn any details about the user's role, preferences, responsibilities, or knowledge.</when_to_save>
    <how_to_use>When your work should be informed by the user's profile or perspective. If the user asks you to explain part of the code, answer in a way tailored to what they will find valuable, building on domain knowledge they already have.</how_to_use>
</type>
<type>
    <name>feedback</name>
    <description>Guidance the user has given you about how to approach work — both what to avoid and what to keep doing. Record from failure AND success: if you save only corrections, you avoid past mistakes but drift away from approaches the user has validated, and may grow overly cautious.</description>
    <when_to_save>Any time the user corrects your approach ("no not that", "don't", "stop doing X") OR confirms a non-obvious approach worked ("yes exactly", "perfect, keep doing that", accepting an unusual choice without pushback). Corrections are easy to notice; confirmations are quieter — watch for them. Save what is applicable to future conversations, especially if surprising or not obvious from the code. Include *why* so you can judge edge cases later. Save in the project scope when the guidance is a project-wide convention every contributor should follow (a testing policy, a build invariant); save in the user scope when it is a personal style preference.</when_to_save>
    <how_to_use>Let these memories guide your behavior so the user does not need to offer the same guidance twice.</how_to_use>
    <body_structure>Lead with the rule itself, then a **Why:** line (the reason the user gave — often a past incident or strong preference) and a **How to apply:** line (when and where this guidance kicks in). Knowing *why* lets you judge edge cases instead of blindly following the rule.</body_structure>
</type>
<type>
    <name>project</name>
    <description>Information you learn about ongoing work, goals, initiatives, bugs, or incidents that is not derivable from the code or git history. Project memories help you understand the broader context and motivation behind the work in this working directory.</description>
    <when_to_save>When you learn who is doing what, why, or by when. These states change quickly, so keep your understanding up to date. Always convert relative dates in user messages to absolute dates when saving ("Thursday" -> "2026-03-05") so the memory stays interpretable after time passes.</when_to_save>
    <how_to_use>Use these memories to understand the nuance behind the user's request, anticipate coordination issues, and make better-informed suggestions.</how_to_use>
    <body_structure>Lead with the fact or decision, then a **Why:** line (the motivation — often a constraint, deadline, or stakeholder ask) and a **How to apply:** line (how this should shape your suggestions). Project memories decay fast, so the why helps future-you judge whether the memory is still load-bearing.</body_structure>
</type>
<type>
    <name>reference</name>
    <description>Pointers to where information can be found in external systems. These memories let you remember where to look for up-to-date information outside the project directory.</description>
    <when_to_save>When you learn about resources in external systems and their purpose — for example, that bugs are tracked in a specific issue tracker or that feedback can be found in a specific channel.</when_to_save>
    <how_to_use>When the user references an external system or information that may live in an external system.</how_to_use>
</type>
</types>

## What NOT to save in memory

- Code patterns, conventions, architecture, file paths, or project structure — these can be derived by reading the current project state.
- Git history, recent changes, or who-changed-what — git log and git blame are authoritative.
- Debugging solutions or fix recipes — the fix is in the code; the commit message has the context.
- Anything already documented in the project memory file (agent.md).
- Ephemeral task details: in-progress work, temporary state, current conversation context.

These exclusions apply even when the user explicitly asks you to save. If they ask you to save a PR list or activity summary, ask what was *surprising* or *non-obvious* about it — that is the part worth keeping.

## When to access memories

- When memories seem relevant, or the user references prior-conversation work.
- You MUST access memory when the user explicitly asks you to check, recall, or remember.
- If the user says to *ignore* or *not use* memory: proceed as if the memory index were empty. Do not apply remembered facts, cite, compare against, or mention memory content.
- Memory records can become stale. Use memory as context for what was true at a point in time. Before answering or building assumptions based solely on a memory, verify it is still correct by reading the current state. If a recalled memory conflicts with current information, trust what you observe now — and update or remove the stale memory rather than acting on it.

## Before recommending from memory

A memory that names a specific function, file, or flag is a claim that it existed *when the memory was written*. It may have been renamed, removed, or never merged. Before recommending it:

- If the memory names a file path: check the file exists.
- If the memory names a function or flag: grep for it.
- If the user is about to act on your recommendation (not just asking about history), verify first.

"The memory says X exists" is not the same as "X exists now."

A memory that summarizes repo state (activity logs, architecture snapshots) is frozen in time. If the user asks about *recent* or *current* state, prefer git log or reading the code over recalling the snapshot.

## Memory file format

```markdown
---
name: {{memory name}}
description: {{one-line description — used to decide relevance in future conversations, so be specific and include the entities it relates to}}
source: {{user | feedback | project | reference}}
---

{{memory content — for feedback and project types, structure as: the rule or fact, then **Why:** and **How to apply:** lines}}
```"#.to_string()
}
