//! ContextBuilder: compose the served view per section (SystemPrompt / Tools /
//! Memory / Skills / Messages), each measurable. This is the unified
//! composition point — system prompt assembly + user-context prepend + the
//! query-loop projection + the API normalize stage — with per-section
//! measurability; here it is one type so /context can break down what the
//! model will see and the agent loop can size it pre-flight.
//!
//! Skeleton (M0 sub-tasks 1-2): defines Section / ServedView / ContextBuilder
//! plus a local tiktoken tokenizer. The Messages section reuses the flat event
//! projection; SystemPrompt / Tools / Memory / Skills sections fill in
//! incrementally (memory provider, AGENTS.md injection, tool schemas)
//! in later slices. Token counts come from tiktoken, not chars/4.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use houyicoder_context::{
    CheckpointManifest, MemoryEntry, TurnEvent, TurnEventKind, memory_age_days, memory_age_label,
    memory_freshness_text,
};
use houyicoder_protocol::llm::{AssistantToolCall, InputItem};

use super::projection;
use super::prompt;
use super::retention;
use super::turn_group;

/// Token budget for per-turn memory recall. Keeps the recalled-memory
/// attachment bounded so it never dominates the served view. Five entries at
/// roughly 400 tokens each fit comfortably; the recall ranker truncates to
/// five before packing against this budget. Pub(crate) so the turn-entry
/// recall step in the runner references one source.
pub(crate) const MEMORY_RECALL_BUDGET: usize = 2000;

/// A measurable section of the served view. /context renders one row per
/// section kind; the agent loop sums the tokens for a pre-flight size check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub kind: SectionKind,
    pub tokens: u32,
    /// Human-readable previews / item labels for /context drill-down.
    pub items: Vec<String>,
}

/// The five sections of the served view. Memory here is the always-on identity
/// (AGENTS.md style) plus recalled memory entries; Skills are the skill
/// frontmatter the model sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    SystemPrompt,
    Tools,
    Memory,
    Skills,
    Messages,
}

impl SectionKind {
    pub fn label(self) -> &'static str {
        match self {
            SectionKind::SystemPrompt => "System prompt",
            SectionKind::Tools => "System tools",
            SectionKind::Memory => "Memory files",
            SectionKind::Skills => "Skills",
            SectionKind::Messages => "Messages",
        }
    }

    /// The 256-color hint the /context grid uses for this section. Matches the
    /// stub palette so the real breakdown renders in the same colors.
    pub fn color_hint(self) -> u8 {
        match self {
            SectionKind::SystemPrompt => 244,
            SectionKind::Tools => 244,
            SectionKind::Memory => 203,
            SectionKind::Skills => 221,
            SectionKind::Messages => 61,
        }
    }
}

/// The assembled view served to the provider: the system prompt, the tool
/// declarations, the projected message history, and a per-section breakdown for
/// /context. The breakdown carries the token count of each section so the
/// served view is measured before it is sent (built into composition, not a
/// separate after-the-fact analyzer).
#[derive(Debug, Clone, Default)]
pub struct ServedView {
    pub system: String,
    pub tools: Vec<String>,
    pub messages: Vec<InputItem>,
    pub sections: Vec<Section>,
}

impl ServedView {
    /// Total served tokens across all sections (pre-flight size).
    pub fn token_count(&self) -> u32 {
        self.sections.iter().map(|s| s.tokens).sum()
    }

    /// Build the /context breakdown from the served sections: one category per
    /// section kind (label + color + tokens) plus a trailing Free-space category
    /// for the unused window, and a proportional grid. The host renders this
    /// directly — no chars/4 estimate, the real per-section token counts from
    /// the assembled view. Category + grid shape, measured pre-flight
    /// (before the call) rather than after.
    pub fn breakdown(&self, model: &str, context_window: u32) -> ContextBreakdown {
        let total: u32 = self.token_count();
        let mut categories: Vec<CategoryBreakdown> = self
            .sections
            .iter()
            .map(|s| CategoryBreakdown {
                label: s.kind.label().to_string(),
                color_hint: s.kind.color_hint(),
                tokens: s.tokens,
                is_deferred: false,
                is_reserved: false,
            })
            .collect();
        let free = context_window.saturating_sub(total);
        if free > 0 {
            categories.push(CategoryBreakdown {
                label: "Free space".to_string(),
                color_hint: 245,
                tokens: free,
                is_deferred: false,
                is_reserved: false,
            });
        }
        let grid = build_grid(&categories, context_window, 80);
        ContextBreakdown {
            model: model.to_string(),
            total_tokens: total,
            context_window,
            categories,
            grid,
            cache_breakpoint: None,
            compact_summary: None,
            cache_prefix_tokens: None,
            cache_hit_rate: None,
        }
    }

    /// Find a section by kind (e.g. the Messages section, for /context).
    pub fn section(&self, kind: SectionKind) -> Option<&Section> {
        self.sections.iter().find(|s| s.kind == kind)
    }
}

/// Local tokenizer (tiktoken) for pre-flight section + served-view sizing. A
/// local BPE keeps /context precise and offline-robust (no API call, no
/// chars/4 CJK error). Approximate for non-tiktoken-native models, but
/// consistent.
pub struct Tokenizer {
    bpe: Option<&'static tiktoken_rs::CoreBPE>,
}

// The BPE vocab is built once and shared across all Tokenizer instances: the
// encoder is read-only after construction and encode_ordinary takes &self, so
// a static reference is safe and avoids rebuilding the ~300 ms table per call.
static BPE: std::sync::OnceLock<tiktoken_rs::CoreBPE> = std::sync::OnceLock::new();

impl Tokenizer {
    pub fn new() -> Self {
        // Under HOUYICODER_FAST_TOKENS (set by run_tests.py for the test run)
        // skip the ~300ms BPE load and use a char-based estimate: tests do not
        // assert on token counts (they assert turns/outcomes), and the load is
        // paid per binary otherwise. Production never sets the env -> real BPE.
        if std::env::var("HOUYICODER_FAST_TOKENS").is_ok() {
            return Self { bpe: None };
        }
        Self::real()
    }

    /// Real tiktoken BPE, always loaded. Used by the accuracy tests that
    /// assert on token counts (CJK undercount etc.) regardless of the fast
    /// env flag.
    pub fn real() -> Self {
        // o200k (the newer encoding) is the better code-aware default; cl100k
        // is the fallback if o200k is unavailable on this build.
        let bpe = BPE.get_or_init(|| {
            tiktoken_rs::o200k_base()
                .or_else(|_| tiktoken_rs::cl100k_base())
                .expect("a bundled tiktoken vocab (o200k or cl100k) must be available")
        });
        Self { bpe: Some(bpe) }
    }

    /// Token count of a string.
    pub fn count(&self, text: &str) -> u32 {
        match self.bpe {
            Some(bpe) => bpe.encode_ordinary(text).len() as u32,
            // Fast path: ~4 chars per token (English) is the standard estimate.
            // Tests do not assert on counts under the fast flag.
            None => (text.chars().count() as u32).div_ceil(4),
        }
    }

    /// Token count of a projected input item: its text plus any tool call names
    /// and inputs, or the tool result output.
    pub fn count_input(&self, item: &InputItem) -> u32 {
        match item {
            InputItem::User { content } => self.count(content),
            InputItem::Assistant {
                content,
                tool_calls,
            } => {
                let mut t = self.count(content);
                for c in tool_calls {
                    t += self.count_assistant_tool_call(c);
                }
                t
            }
            InputItem::ToolResult { output, .. } => self.count(&output.to_string()),
        }
    }

    fn count_assistant_tool_call(&self, c: &AssistantToolCall) -> u32 {
        self.count(&c.name) + self.count(&c.input.to_string())
    }
}

impl Default for Tokenizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Compose the served view for a turn. The skeleton reuses the flat event
/// projection for the Messages section; the SystemPrompt section is assembled
/// from an identity + project-context (AGENTS.md) walk-up + tool-docs + env
/// stub. Tools / Memory / Skills sections fill in incrementally in later
/// slices.
pub struct ContextBuilder {
    tokenizer: Tokenizer,
    /// Interior-mutable so a worktree session can switch the project-context
    /// walk-up cwd at runtime through a shared Arc<Runner> (the worktree
    /// feature narrows the fence and repoints the cwd to the worktree path).
    cwd: Arc<RwLock<PathBuf>>,
    /// The most recently built served view, cached so the host can render
    /// /context from the exact view the model saw (no separate analyzer pass).
    last_served: Mutex<Option<ServedView>>,
    /// The retention policy for the served view's block_ref ToolResults. When
    /// set, the cache-liveness policy holds per-block decisions stable while
    /// the cached prefix is live; unset falls back to the age-based 3-tier
    /// (tests + the stub path). Interior-mutable so the Runner can install it
    /// post-construction with the shared cached-prefix state.
    retention_policy: Mutex<Option<Arc<dyn retention::RetentionPolicy>>>,
    /// The agent directory section (deterministic list of registered agent
    /// types the model may delegate to), injected into the system prompt so
    /// the model can discover sub-agent types. Interior-mutable so the
    /// composition root installs it post-construction.
    agent_directory: Mutex<Option<String>>,
}

impl ContextBuilder {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            tokenizer: Tokenizer::new(),
            cwd: Arc::new(RwLock::new(cwd)),
            last_served: Mutex::new(None),
            retention_policy: Mutex::new(None),
            agent_directory: Mutex::new(None),
        }
    }

    /// Install the agent directory section for the system prompt. Set once
    /// at the composition root (the registry is fixed for the session).
    pub(crate) fn set_agent_directory(&self, section: String) {
        if let Ok(mut g) = self.agent_directory.lock() {
            *g = Some(section);
        }
    }

    pub(crate) fn agent_directory(&self) -> Option<String> {
        self.agent_directory.lock().ok().and_then(|g| g.clone())
    }

    /// Install the retention policy the serve path uses for block_ref
    /// ToolResults. The Runner calls this at construction with the cache-
    /// liveness policy sharing its cached-prefix state; the serve path then
    /// holds per-block decisions stable while the cached prefix is live.
    pub(crate) fn set_retention_policy(&self, policy: Arc<dyn retention::RetentionPolicy>) {
        if let Ok(mut g) = self.retention_policy.lock() {
            *g = Some(policy);
        }
    }

    /// Override the cwd used for the memory-file walk-up (tests + harness that
    /// pins a workspace root).
    pub fn with_cwd(self, cwd: PathBuf) -> Self {
        *self.cwd.write().expect("cwd lock") = cwd;
        self
    }

    /// Switch the cwd at runtime (worktree enter/exit). Writes the cwd and
    /// clears the cached served view so the next build recomputes the system
    /// prompt with the new project context (AGENTS.md walk-up). Clears the
    /// cwd-dependent system-prompt + memory-file caches on worktree entry.
    pub fn switch_cwd(&self, cwd: PathBuf) {
        *self.cwd.write().expect("cwd lock") = cwd;
        if let Ok(mut g) = self.last_served.lock() {
            *g = None;
        }
    }

    /// A shared handle to the interior-mutable cwd, so a WorktreeController
    /// can switch it without a typed Runner handle (the controller writes the
    /// Arc directly). The build path reads through the same Arc.
    pub fn cwd_handle(&self) -> Arc<RwLock<PathBuf>> {
        Arc::clone(&self.cwd)
    }

    /// Build the served view from a session's event log. The system prompt is
    /// assembled from sections (byte-stable across turns unless the memory file
    /// changes); the Messages section is the flat projection of the event log.
    pub fn build(&self, events: &[TurnEvent]) -> ServedView {
        self.build_with_manifest(events, None, None, &[], None)
    }

    /// Build the served view, optionally applying a CheckpointManifest to the
    /// event log before projection. When manifest is None, the behavior is
    /// identical to build() (full replay, no plan applied). When Some, the
    /// manifest's per-event Disposition is applied: Verbatim events stay,
    /// Summarized events fold into the summary, Referenced ToolResult outputs
    /// are externalized to the CAS (when a backend is provided) or kept
    /// as-is (fail-closed). This is the Select stage's plan-application step.
    ///
    /// The system prompt is the frozen identity/project prompt with no recalled
    /// memory in it — memory lands as a durable memory-recall attachment in the
    /// message stream (merged into the user turn by the projection), so the
    /// system prompt stays byte-stable across turns for prompt-cache. The
    /// Memory section here only measures the attachment tokens already in the
    /// served view (for /context); it does not inject anything.
    pub fn build_with_manifest(
        &self,
        events: &[TurnEvent],
        manifest: Option<&CheckpointManifest>,
        backend: Option<&dyn houyicoder_context::ContextBackend>,
        tool_defs: &[houyicoder_protocol::llm::ToolDef],
        memory_index: Option<&str>,
    ) -> ServedView {
        let filtered = match manifest {
            Some(m) => projection::apply_manifest(events, m, backend),
            None => events.to_vec(),
        };
        // When the cache-liveness policy is installed, serve with it + the
        // current wall clock so per-block decisions stay stable while the
        // cached prefix is live. Otherwise the age-based default applies
        // (now=0 never reports a live cache, so no stability is held).
        let messages = match self.retention_policy.lock().ok().and_then(|g| g.clone()) {
            Some(policy) => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                turn_group::project_input_items_with(&filtered, backend, &*policy, now_ms)
            }
            None => turn_group::project_input_items(&filtered, backend),
        };
        let msg_tokens: u32 = messages.iter().map(|m| self.tokenizer.count_input(m)).sum();

        let cwd = self.cwd.read().expect("cwd lock").clone();
        let agent_directory = self.agent_directory.lock().ok().and_then(|g| g.clone());
        let prompt = prompt::SystemPrompt::build_with_memory_index(
            &cwd,
            memory_index,
            agent_directory.as_deref(),
        );

        // Measure the recalled-memory attachment already in the served view:
        // the memory-recall events the turn-entry step appended, which the
        // projection merged into the user turn. Recomputed from the projection
        // so /context reflects what the model sees without a second recall.
        let mut mem_tokens = 0u32;
        let mut mem_items = Vec::new();
        let mut skill_tokens = 0u32;
        for ev in &filtered {
            match &ev.kind {
                TurnEventKind::MemoryRecall { text, keys, .. } => {
                    mem_tokens += self.tokenizer.count(text);
                    mem_items.extend(keys.iter().cloned());
                }
                TurnEventKind::SkillListing { text, .. } => {
                    skill_tokens += self.tokenizer.count(text);
                }
                _ => {}
            }
        }

        // The memory + skill-listing text is merged into the user message by
        // the projection, so it is already counted in msg_tokens. Attribute
        // each to its own section and subtract from Messages so the section
        // totals do not double-count — otherwise the pre-flight compress
        // threshold trips about an attachment's worth of tokens early.
        let messages_section = Section {
            kind: SectionKind::Messages,
            tokens: msg_tokens
                .saturating_sub(mem_tokens)
                .saturating_sub(skill_tokens),
            items: message_previews(&messages),
        };

        // Tool schemas occupy context window (sent as a separate API param
        // but the window budget counts them). Count them here so the
        // pre-flight gate does not underestimate by 3-8k/turn — a tool-heavy
        // session tripped the gate late because tool_defs were invisible to
        // token_count (R11a).
        let tool_tokens: u32 = tool_defs
            .iter()
            .map(|td| {
                self.tokenizer
                    .count(&serde_json::to_string(td).unwrap_or_default())
            })
            .sum();
        let mut sections = vec![
            Section {
                kind: SectionKind::SystemPrompt,
                tokens: self.tokenizer.count(&prompt.text),
                items: prompt.items,
            },
            messages_section,
        ];
        if !tool_defs.is_empty() {
            sections.insert(
                1,
                Section {
                    kind: SectionKind::Tools,
                    tokens: tool_tokens,
                    items: tool_defs.iter().map(|td| td.name.clone()).collect(),
                },
            );
        }
        if mem_tokens > 0 {
            sections.insert(
                1,
                Section {
                    kind: SectionKind::Memory,
                    tokens: mem_tokens,
                    items: mem_items,
                },
            );
        }
        if skill_tokens > 0 {
            sections.insert(
                1,
                Section {
                    kind: SectionKind::Skills,
                    tokens: skill_tokens,
                    items: vec!["skill listing".to_string()],
                },
            );
        }

        let served = ServedView {
            system: prompt.text,
            tools: tool_defs.iter().map(|td| td.name.clone()).collect(),
            messages,
            sections,
        };
        // Cache the built view so /context renders exactly what the model saw
        // without a separate analyzer pass — measurement is built into
        // composition, not a separate after-the-fact analyzer.
        if let Ok(mut g) = self.last_served.lock() {
            *g = Some(served.clone());
        }
        served
    }

    /// The most recently built served view, so the host can render /context
    /// from the exact view the model saw. None before the first turn builds one.
    pub fn last_served(&self) -> Option<ServedView> {
        self.last_served.lock().ok().and_then(|g| g.clone())
    }

    /// The tokenizer used for section sizing (shared with /context).
    pub fn tokenizer(&self) -> &Tokenizer {
        &self.tokenizer
    }
}

impl Default for ContextBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// One-line previews of the projected messages, for /context drill-down.
fn message_previews(messages: &[InputItem]) -> Vec<String> {
    messages
        .iter()
        .map(|m| match m {
            InputItem::User { content } => preview(content),
            InputItem::Assistant { content, .. } => preview(content),
            InputItem::ToolResult { output, .. } => preview(&output.to_string()),
        })
        .collect()
}

/// Truncate a string to a char budget with an ellipsis when cut.
fn preview(s: &str) -> String {
    const MAX: usize = 60;
    let chars: Vec<char> = s.chars().take(MAX + 1).collect();
    if chars.len() <= MAX {
        return s.to_string();
    }
    let mut t: String = chars[..MAX].iter().collect();
    t.push('\u{2026}');
    t
}

/// Render recalled entries into a system-reminder-wrapped attachment the
/// turn-entry step appends as a memory-recall event. The wrapper marks the
/// content as injected context (transient, not a user instruction) so the
/// model treats it as transient context; the projection then merges this
/// text into the turn's user message so one user turn carries the query
/// plus its recalled-memory attachment.
///
/// Each entry renders as a manifest header in scan format: dash, type tag
/// in brackets, key, age in parentheses, then the one-line description
/// hook. The body content follows (phase-three read — the model gets usable
/// context with no second disk read; a path-only return would force a
/// second read). Entries older than a day get a
/// staleness caveat so the model verifies against current code rather than
/// asserting stale file:line claims as fact.
pub(crate) fn render_recall_text(entries: &[MemoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut text = String::from("<system-reminder>\n# Recalled memories\n");
    for entry in entries {
        let age_days = memory_age_days(entry.mtime_secs, now_secs);
        let age_label = memory_age_label(age_days);
        let header = if entry.description.is_empty() {
            format!(
                "- [{}] {} ({})\n",
                entry.source.as_label(),
                entry.key,
                age_label
            )
        } else {
            format!(
                "- [{}] {} ({}): {}\n",
                entry.source.as_label(),
                entry.key,
                age_label,
                entry.description
            )
        };
        text.push_str(&header);
        text.push_str(&entry.content);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        let caveat = memory_freshness_text(age_days);
        if !caveat.is_empty() {
            text.push_str(&caveat);
            text.push('\n');
        }
    }
    text.push_str("</system-reminder>");
    text
}

// ===== /context breakdown (interface-first) =====
// The data the /context viz renders. Decoupled from core: a stub mock fills it
// now; the real ContextBuilder produces it once the served view is sectioned.
// Grid construction: read directly + 7-dim review-fixed: cache is a
// breakpoint index not per-category; tiktoken is local-estimated not
// precise; glyphs are real Unicode in the ratatui render.

// The context grid types + build_grid live in protocol (the wire crate) —
// they are serialized + sent to clients. Core's copies were verbatim
// duplicates (same fields, fewer derives); protocol is the canonical owner.
// Re-export so callers of crate::agent::context::* still resolve.
pub use houyicoder_protocol::frontend::context::{
    CategoryBreakdown, ContextBreakdown, GridSquare, build_grid,
};

/// A canned ContextBreakdown for the no-runner / preview path so the /context
/// viz renders a real-shaped grid before the real ContextBuilder is wired. The
/// numbers reflect a typical session footprint; the grid is built via build_grid.
pub fn stub_breakdown() -> ContextBreakdown {
    let window: u32 = 200_000;
    let cats: Vec<CategoryBreakdown> = vec![
        CategoryBreakdown {
            label: "System prompt".into(),
            color_hint: 244,
            tokens: 1_800,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "System tools".into(),
            color_hint: 244,
            tokens: 19_000,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Memory files".into(),
            color_hint: 203,
            tokens: 2_500,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Skills".into(),
            color_hint: 221,
            tokens: 1_800,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Messages".into(),
            color_hint: 61,
            tokens: 120_000,
            is_deferred: false,
            is_reserved: false,
        },
        CategoryBreakdown {
            label: "Free space".into(),
            color_hint: 245,
            tokens: window - 145_100,
            is_deferred: false,
            is_reserved: false,
        },
    ];
    let total: u32 = cats.iter().map(|c| c.tokens).sum();
    let grid = build_grid(&cats, window, 100);
    ContextBreakdown {
        model: "glm-5.2".into(),
        total_tokens: total,
        context_window: window,
        categories: cats,
        grid,
        cache_breakpoint: None,
        compact_summary: None,
        cache_prefix_tokens: None,
        cache_hit_rate: None,
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
