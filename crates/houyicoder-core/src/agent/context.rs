//! Builds and measures provider-facing model context.
//!
//! Checkpoint selection and retention precede message assembly. The resulting
//! section measurements drive pre-flight limits and the /context view.

pub(crate) mod cache_liveness;
pub(crate) mod prompt;
pub(crate) mod retention;
pub(crate) mod selection;
pub(crate) mod turn_group;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use houyicoder_context::{
    CheckpointManifest, MemoryEntry, SessionEvent, SessionLogEntry, memory_age_days,
    memory_age_label, memory_freshness_text,
};
use houyicoder_protocol::llm::{AssistantToolCall, InputItem};

/// Per-turn recall budget shared by retrieval and context assembly.
pub(crate) const MEMORY_RECALL_BUDGET: usize = 2000;

/// Measured section of the model context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub kind: SectionKind,
    pub tokens: u32,
    /// Human-readable previews / item labels for /context drill-down.
    pub items: Vec<String>,
}

/// Sections contributing to the model's context window.
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

    /// Palette index for the /context grid.
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

/// One context assembly's product: the model-facing context (system +
/// messages) and its pre-flight measurement.
#[derive(Debug, Clone, Default)]
pub struct AssembledContext {
    pub system: String,
    /// Byte offset into system where the cwd-dependent dynamic suffix begins.
    /// The static prefix before it is what the prompt-cache key treats as
    /// stable; carried out of assembly so the request builder can thread it to
    /// the provider instead of dropping it here.
    pub dynamic_boundary: usize,
    pub messages: Vec<InputItem>,
    pub measurement: ContextMeasurement,
}

/// Pre-flight section measurements of an assembled context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextMeasurement {
    pub tool_names: Vec<String>,
    pub sections: Vec<Section>,
}

impl ContextMeasurement {
    /// Total tokens across all context sections.
    pub fn token_count(&self) -> u32 {
        self.sections.iter().map(|s| s.tokens).sum()
    }

    /// Build the /context categories and proportional grid.
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
            memory_files: Vec::new(),
            skills: Vec::new(),
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

/// Local BPE tokenizer for deterministic pre-flight measurement.
pub struct Tokenizer {
    bpe: Option<&'static tiktoken_rs::CoreBPE>,
}

// The immutable BPE table is shared because construction is expensive.
static BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();

impl Tokenizer {
    pub fn new() -> Self {
        // The test harness bypasses BPE construction unless accuracy is under test.
        if var("HOUYICODER_FAST_TOKENS").is_ok() {
            return Self { bpe: None };
        }
        Self::real()
    }

    /// Construct with the real BPE regardless of the test fast path.
    pub fn real() -> Self {
        // Prefer the newer code-aware vocabulary, with a bundled fallback.
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

    /// Token count of an assembled input item: its text plus any tool call names
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

/// Assembles model context from durable events and runtime capabilities.
pub struct ContextBuilder {
    tokenizer: Tokenizer,
    /// Runtime cwd shared with worktree switching.
    cwd: Arc<RwLock<PathBuf>>,
    /// Last turn's measurement, cached for /context. Only build_for_turn
    /// writes; build() does not.
    last_measurement: Mutex<Option<ContextMeasurement>>,
    /// Retention policy shared with cached-prefix liveness.
    retention_policy: Mutex<Option<Arc<dyn retention::RetentionPolicy>>>,
    /// The agent directory section (deterministic list of registered agent
    /// types the model may delegate to), injected into the system prompt so
    /// the model can discover sub-agent types. Interior-mutable so the
    /// composition root installs it post-construction.
    agent_directory: Mutex<Option<String>>,
}

impl ContextBuilder {
    pub fn new() -> Self {
        let cwd = current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            tokenizer: Tokenizer::new(),
            cwd: Arc::new(RwLock::new(cwd)),
            last_measurement: Mutex::new(None),
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
    /// clears the cached measurement so the next build recomputes the system
    /// prompt with the new project context (AGENTS.md walk-up). Clears the
    /// cwd-dependent system-prompt + memory-file caches on worktree entry.
    pub fn switch_cwd(&self, cwd: PathBuf) {
        *self.cwd.write().expect("cwd lock") = cwd;
        if let Ok(mut g) = self.last_measurement.lock() {
            *g = None;
        }
    }

    /// A shared handle to the interior-mutable cwd, so a WorktreeController
    /// can switch it without a typed Runner handle (the controller writes the
    /// Arc directly). The build path reads through the same Arc.
    pub fn cwd_handle(&self) -> Arc<RwLock<PathBuf>> {
        Arc::clone(&self.cwd)
    }

    /// Bare assembly: no checkpoint selection, no tool measurement, no
    /// memory index. Does not cache. Used by tests.
    pub fn build(&self, events: &[SessionLogEntry]) -> AssembledContext {
        self.assemble(events, None, None, &[], None)
    }

    /// Prospective assembly: what the first turn would measure, with the real
    /// tool definitions and the memory index the turn path passes, so a
    /// /context before any turn reports the same system prompt and tools the
    /// first turn will send. Does not cache: last_measurement stays None
    /// until a turn actually runs.
    pub fn build_prospective(
        &self,
        tool_defs: &[ToolDef],
        memory_index: Option<&str>,
    ) -> AssembledContext {
        self.assemble(&[], None, None, tool_defs, memory_index)
    }

    /// The turn path: select events per the checkpoint manifest, assemble,
    /// measure real tool defs and the memory index, and cache the
    /// measurement. Recalled memory stays in the message stream so the
    /// system prompt stays cache-stable.
    pub fn build_for_turn(
        &self,
        events: &[SessionLogEntry],
        manifest: Option<&CheckpointManifest>,
        backend: Option<&dyn houyicoder_context::ContextBackend>,
        tool_defs: &[ToolDef],
        memory_index: Option<&str>,
    ) -> AssembledContext {
        let assembled = self.assemble(events, manifest, backend, tool_defs, memory_index);
        if let Ok(mut g) = self.last_measurement.lock() {
            *g = Some(assembled.measurement.clone());
        }
        assembled
    }

    fn assemble(
        &self,
        events: &[SessionLogEntry],
        manifest: Option<&CheckpointManifest>,
        backend: Option<&dyn houyicoder_context::ContextBackend>,
        tool_defs: &[ToolDef],
        memory_index: Option<&str>,
    ) -> AssembledContext {
        let filtered = match manifest {
            Some(manifest) => selection::apply_manifest(events, manifest, backend),
            None => events.to_vec(),
        };
        let messages = self.assemble_messages(&filtered, backend);
        let msg_tokens: u32 = messages
            .iter()
            .map(|message| self.tokenizer.count_input(message))
            .sum();

        let cwd = self.cwd.read().expect("cwd lock").clone();
        let agent_directory = self.agent_directory.lock().ok().and_then(|g| g.clone());
        let prompt =
            SystemPrompt::build_with_memory_index(&cwd, memory_index, agent_directory.as_deref());

        // Attribute attachments already merged into the assembled messages.
        let mut mem_tokens = 0u32;
        let mut mem_items = Vec::new();
        let mut skill_tokens = 0u32;
        for ev in &filtered {
            match &ev.event {
                SessionEvent::MemoryRecall { text, keys, .. } => {
                    mem_tokens += self.tokenizer.count(text);
                    mem_items.extend(keys.iter().cloned());
                }
                SessionEvent::SkillListing { text, .. } => {
                    skill_tokens += self.tokenizer.count(text);
                }
                _ => {}
            }
        }

        // Subtract attachments from Messages to keep section totals disjoint.
        let messages_section = Section {
            kind: SectionKind::Messages,
            tokens: msg_tokens
                .saturating_sub(mem_tokens)
                .saturating_sub(skill_tokens),
            items: message_previews(&messages),
        };

        // Tool schemas consume context despite traveling outside messages.
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

        let measurement = ContextMeasurement {
            tool_names: tool_defs.iter().map(|td| td.name.clone()).collect(),
            sections,
        };
        AssembledContext {
            system: prompt.text,
            dynamic_boundary: prompt.dynamic_boundary,
            messages,
            measurement,
        }
    }

    fn assemble_messages(
        &self,
        events: &[SessionLogEntry],
        backend: Option<&dyn houyicoder_context::ContextBackend>,
    ) -> Vec<InputItem> {
        let policy = self
            .retention_policy
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        let Some(policy) = policy else {
            return turn_group::assemble_model_input(events, backend);
        };
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        turn_group::assemble_model_input_with(events, backend, &*policy, now_ms)
    }

    /// The measurement of the most recent turn. None before the first turn.
    /// Not the exact provider request — call.rs appends configured
    /// instructions to the system prompt after assembly.
    pub fn last_measurement(&self) -> Option<ContextMeasurement> {
        self.last_measurement.lock().ok().and_then(|g| g.clone())
    }

    /// Drop the cached measurement so the next build recomputes it, keeping a
    /// cleared session from reporting the size of a context that is gone.
    pub fn clear_measurement(&self) {
        if let Ok(mut g) = self.last_measurement.lock() {
            *g = None;
        }
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

/// One-line previews of the assembled messages, for /context drill-down.
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

/// Render recalled memories as untrusted model context.
///
/// Entries include source, age, description, content, and a freshness warning
/// when current code should be rechecked.
pub(crate) fn render_recall_text(entries: &[MemoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
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

// Protocol owns the serialized context breakdown types.
pub use houyicoder_protocol::frontend::context::{
    CategoryBreakdown, ContextBreakdown, GridCell, build_grid,
};
use houyicoder_protocol::llm::ToolDef;
use prompt::SystemPrompt;
use std::env::current_dir;
use std::env::var;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[cfg(test)]
mod tests;
