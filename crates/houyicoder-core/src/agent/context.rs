//! Builds and measures provider-facing model context.
//!
//! Checkpoint selection and retention precede message assembly. The resulting
//! section measurements drive pre-flight limits and the /context view.
//!
//! The sections partition the request instead of overlapping it: the memory
//! blocks the system prompt carries are charged to the memory section and
//! subtracted from the prompt's own footprint, and the skill listing the
//! message stream carries is charged to the skills section and subtracted from
//! the messages. The /context drill-down rows come from the same pass, so the
//! legend and the detail beneath it describe one measurement.
//!
//! Two kinds of figure come out of a pass. The sections describe what the
//! request pays for, and every path that builds one reads them. The
//! conversation cost reads the whole assembled history, so only the
//! measurement behind the /context pane derives it.

pub(crate) mod cache_liveness;
pub(crate) mod conversation;
pub(crate) mod prompt;
pub(crate) mod retention;
pub(crate) mod selection;
pub(crate) mod turn_group;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use houyicoder_context::{
    CheckpointManifest, MemoryEntry, SessionEvent, SessionLogEntry, memory_age_days,
    memory_age_label, memory_freshness_text,
};
use houyicoder_protocol::frontend::context::{DelegatedContext, ToolFootprint};
use houyicoder_protocol::llm::{AssistantToolCall, InputItem};

use crate::agent::context::conversation::ConversationCost;
use crate::agent::multi_agent::registry::{AgentDefinition, agent_directory_section};

/// Per-turn recall budget shared by retrieval and context assembly.
pub(crate) const MEMORY_RECALL_BUDGET: usize = 2000;

/// The capability-derived inputs one assembly measures: the tool schemas the
/// request declares, the frozen memory index the prompt carries, the skills
/// the listing announces, and every skill discovery found. The turn path and
/// the /context measurement build the same set, so a reported breakdown
/// describes the request that would be sent.
///
/// The announced skills are a subset of the discovered ones: the /context
/// rows cover discovery so a skill the listing does not announce still shows, and
/// only the announced part is counted, because only the announced part reaches
/// the window.
#[derive(Debug, Clone, Copy, Default)]
pub struct AssemblyInput<'a> {
    pub tool_defs: &'a [ToolDef],
    pub memory_index: Option<&'a str>,
    pub announced_skills: &'a [SkillSnapshot],
    pub discovered_skills: &'a [SkillSnapshot],
}

/// Measured section of the model context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub kind: SectionKind,
    pub tokens: u32,
}

/// Sections contributing to the model's context window, in the order the
/// legend and the grid present them.
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
            SectionKind::SystemPrompt => 39,
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
    /// Tokens of the whole system prompt text, the memory blocks included. The
    /// sections split that footprint; the cache prefix the request repeats
    /// across turns is this plus the tools.
    pub system_prompt_tokens: u32,
    /// The memory rows the /context drill-down lists, one per block the prompt
    /// carries plus one for the recalled memories in the message stream. Their
    /// tokens sum to the memory section.
    pub memory_files: Vec<ContextFileEntry>,
    /// The skill rows the /context drill-down lists, one per configured skill.
    /// The rows split the skills section, so they sum to it whenever a skill is
    /// configured: before the first announcement the section is the projected
    /// size of the entries, and from then on it is the listing the message
    /// stream carries. Only a listing with no configured skill to split it
    /// leaves the section non-zero with no rows.
    pub skills: Vec<ContextSkillEntry>,
    pub sections: Vec<Section>,
    /// Per-tool conversation footprints, heaviest first. The /context
    /// suggestions name the tool whose results crowd the window. Empty for a
    /// measurement that did not derive the conversation cost.
    pub tool_footprints: Vec<ToolFootprint>,
    /// What the session's delegated children cost this context. None when the
    /// session ran no child, and for a measurement that did not derive the
    /// conversation cost.
    pub delegated: Option<DelegatedContext>,
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
            memory_files: self.memory_files.clone(),
            skills: self.skills.clone(),
            compact_summary: None,
            cache_prefix_tokens: None,
            cache_hit_rate: None,
            tool_footprints: self.tool_footprints.clone(),
            delegated: self.delegated.clone(),
        }
    }

    /// Find a section by kind (e.g. the Messages section, for /context).
    pub fn section(&self, kind: SectionKind) -> Option<&Section> {
        self.sections.iter().find(|s| s.kind == kind)
    }
}

/// The label for a set of memory entries, singular on a count of one so a row
/// holding one entry does not advertise a plural it does not have.
pub(crate) fn memory_entry_label(count: usize) -> String {
    if count == 1 {
        "1 entry".to_string()
    } else {
        format!("{count} entries")
    }
}

/// Split a measured total across parts in proportion to their own measures, in
/// whole tokens that sum to the total: each part takes its floor, and the parts
/// with the largest remainders take the rest. Used where a measured figure has
/// to agree with a per-item breakdown of it. Measures that are all zero carry
/// no proportion, so the whole total lands on the first part instead of
/// vanishing. An empty list of parts reports nothing: there is no row to carry
/// the total, so the caller decides what a section with no items reports.
fn split_total(total: u32, parts: &[u32]) -> Vec<u32> {
    let sum: u64 = parts.iter().map(|part| u64::from(*part)).sum();
    let mut out: Vec<u32> = Vec::with_capacity(parts.len());
    if sum == 0 {
        out.resize(parts.len(), 0);
        if let Some(first) = out.first_mut() {
            *first = total;
        }
        return out;
    }
    let whole = u64::from(total);
    for part in parts {
        out.push(((whole * u64::from(*part)) / sum) as u32);
    }
    let mut residue = total.saturating_sub(out.iter().sum());
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by(|left, right| {
        let left_rest = (whole * u64::from(parts[*left])) % sum;
        let right_rest = (whole * u64::from(parts[*right])) % sum;
        right_rest.cmp(&left_rest).then(left.cmp(right))
    });
    for index in order {
        if residue == 0 {
            break;
        }
        out[index] += 1;
        residue -= 1;
    }
    out
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

/// The Skills section as one pass sees it: the size the request pays, the share
/// of that size the message stream carries, and the per-skill rows.
struct SkillCost {
    section_tokens: u32,
    stream_tokens: u32,
    rows: Vec<ContextSkillEntry>,
}

/// Assembles model context from durable events and runtime capabilities.
pub struct ContextBuilder {
    tokenizer: Tokenizer,
    /// Runtime cwd shared with worktree switching.
    cwd: Arc<RwLock<PathBuf>>,
    /// Last turn's measurement, cached for the reported occupancy. Only
    /// build_for_turn writes; build() does not. It holds the sections and no
    /// conversation cost.
    last_measurement: Mutex<Option<ContextMeasurement>>,
    /// Retention policy shared with cached-prefix liveness.
    retention_policy: Mutex<Option<Arc<dyn retention::RetentionPolicy>>>,
    /// The sub-agent types the model may delegate to. The system prompt's
    /// directory section is derived from these, so the definitions are the
    /// stored fact and the paragraph is a rendering of it; the /agents reply
    /// reads the same list. Interior-mutable so the composition root installs
    /// them post-construction.
    agent_types: Mutex<Vec<AgentDefinition>>,
    /// The project memory and date line the session froze on its first
    /// assembly. Held so an unrelated filesystem or clock change cannot
    /// rewrite the prompt prefix mid-session.
    frozen_prompt: Mutex<Option<FrozenPrompt>>,
    /// The prefix the last assembly produced. Declared here and owned by the
    /// prompt cache module, which is the only reader and writer.
    prompt_hash: Mutex<Option<PromptHash>>,
}

impl ContextBuilder {
    pub fn new() -> Self {
        let cwd = current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            tokenizer: Tokenizer::new(),
            cwd: Arc::new(RwLock::new(cwd)),
            last_measurement: Mutex::new(None),
            retention_policy: Mutex::new(None),
            agent_types: Mutex::new(Vec::new()),
            frozen_prompt: Mutex::new(None),
            prompt_hash: Mutex::new(None),
        }
    }

    /// Install the delegatable agent definitions. Set once at the composition
    /// root (the registry is fixed for the session).
    pub(crate) fn set_agent_types(&self, defs: Vec<AgentDefinition>) {
        if let Ok(mut g) = self.agent_types.lock() {
            *g = defs;
        }
    }

    /// The installed definitions, in the order they were installed.
    pub(crate) fn agent_types(&self) -> Vec<AgentDefinition> {
        self.agent_types
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
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

    /// Bare assembly: no checkpoint selection and no capability inputs. Does
    /// not cache. Used by tests.
    pub fn build(&self, events: &[SessionLogEntry]) -> AssembledContext {
        self.assemble(
            events,
            None,
            None,
            &AssemblyInput::default(),
            ConversationCost::SectionsOnly,
        )
    }

    /// Measure the assembly these events would produce, without caching it, and
    /// with the conversation cost the /context pane reads. The pane measures
    /// the session's loaded events this way, so a resumed session reports its
    /// history before another turn runs.
    pub fn measure(
        &self,
        events: &[SessionLogEntry],
        manifest: Option<&CheckpointManifest>,
        backend: Option<&dyn houyicoder_context::ContextBackend>,
        input: &AssemblyInput<'_>,
    ) -> AssembledContext {
        self.assemble(
            events,
            manifest,
            backend,
            input,
            ConversationCost::WithFootprints,
        )
    }

    /// The turn path: select events per the checkpoint manifest, assemble,
    /// measure the declared tools, the frozen memory index, and the announced
    /// skills, and cache the measurement. Recalled memory stays in the message
    /// stream so the system prompt stays cache-stable. The conversation cost is
    /// left out: nothing on this path reads it, and deriving it would walk the
    /// whole history on every turn.
    pub fn build_for_turn(
        &self,
        events: &[SessionLogEntry],
        manifest: Option<&CheckpointManifest>,
        backend: Option<&dyn houyicoder_context::ContextBackend>,
        input: &AssemblyInput<'_>,
    ) -> AssembledContext {
        let assembled = self.assemble(
            events,
            manifest,
            backend,
            input,
            ConversationCost::SectionsOnly,
        );
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
        input: &AssemblyInput<'_>,
        cost: ConversationCost,
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
        // Rendered under the lock: the definitions are copied only for the
        // caller that reads them, never per assembly.
        let agent_directory = self
            .agent_types
            .lock()
            .ok()
            .and_then(|defs| agent_directory_section(&defs));
        let freeze = self.frozen_prompt(&cwd);
        let prompt = SystemPrompt::build_with_memory_index(
            &cwd,
            input.memory_index,
            agent_directory.as_deref(),
            &freeze,
        );
        let prompt_tokens = self.tokenizer.count(&prompt.text);

        // The prompt's memory blocks are memory, not prompt framing: charging
        // them here is what makes a project memory file visible as a category
        // of its own instead of hiding inside the prompt's total.
        let memory_files: Vec<ContextFileEntry> = prompt
            .memory_blocks
            .iter()
            .map(|block| ContextFileEntry {
                label: block.label.clone(),
                tokens: self.tokenizer.count(&block.text),
            })
            .collect();
        let block_tokens: u32 = memory_files.iter().map(|row| row.tokens).sum();

        // Recalled memories ride the message stream; they are memory too, and
        // one aggregate row reports them rather than one row per entry.
        let mut recall_tokens = 0u32;
        let mut recall_entries = 0usize;
        for ev in &filtered {
            if let SessionEvent::MemoryRecall { text, keys, .. } = &ev.event {
                recall_tokens += self.tokenizer.count(text);
                recall_entries += keys.len();
            }
        }
        let mut memory_files = memory_files;
        if recall_entries > 0 {
            memory_files.push(ContextFileEntry {
                label: format!("Recalled memories ({})", memory_entry_label(recall_entries)),
                tokens: recall_tokens,
            });
        }
        let memory_tokens = block_tokens + recall_tokens;

        let skills = self.skill_cost(&filtered, input.announced_skills, input.discovered_skills);
        self.record_prompt_hash(&prompt.text, input.tool_defs);

        // Tool schemas consume context despite traveling outside messages.
        let tool_tokens: u32 = input
            .tool_defs
            .iter()
            .map(|td| {
                self.tokenizer
                    .count(&serde_json::to_string(td).unwrap_or_default())
            })
            .sum();

        let sections = vec![
            Section {
                kind: SectionKind::SystemPrompt,
                tokens: prompt_tokens.saturating_sub(block_tokens),
            },
            Section {
                kind: SectionKind::Tools,
                tokens: tool_tokens,
            },
            Section {
                kind: SectionKind::Memory,
                tokens: memory_tokens,
            },
            Section {
                kind: SectionKind::Skills,
                tokens: skills.section_tokens,
            },
            Section {
                kind: SectionKind::Messages,
                tokens: msg_tokens
                    .saturating_sub(recall_tokens)
                    .saturating_sub(skills.stream_tokens),
            },
        ];

        let (tool_footprints, delegated) = self.conversation_cost(&filtered, cost);
        let measurement = ContextMeasurement {
            tool_footprints,
            delegated,
            system_prompt_tokens: prompt_tokens,
            memory_files,
            skills: skills.rows,
            sections,
        };
        AssembledContext {
            system: prompt.text,
            dynamic_boundary: prompt.dynamic_boundary,
            messages,
            measurement,
        }
    }

    /// The Skills section with its per-skill rows and the share of it the
    /// message stream carries, returned together so the rows always split the
    /// section they belong to and the messages are charged exactly what they
    /// hold. The announcements in the message stream are the text the request
    /// pays for, and the listing budget can shorten them, so the section takes
    /// its size from those announcements and each row takes a share of that
    /// size in proportion to its own entry. Before the first announcement the
    /// announced entries alone are what the next request would carry, and that projected
    /// size rides no message, so nothing is subtracted for it.
    ///
    /// One row per discovered skill, not per announced one: a skill the
    /// listing does not announce belongs in the drill-down beside its siblings, so the
    /// reader sees the whole set the registry found, and its cost is zero
    /// because the window carries nothing of it. The announced rows keep the
    /// share split among themselves, so they still sum to the Skills category
    /// the legend reports.
    fn skill_cost(
        &self,
        events: &[SessionLogEntry],
        announced_skills: &[SkillSnapshot],
        discovered: &[SkillSnapshot],
    ) -> SkillCost {
        let entry_tokens: Vec<u32> = announced_skills
            .iter()
            .map(|snapshot| self.tokenizer.count(&listing_entry(&snapshot.descriptor)))
            .collect();
        let stream_tokens: u32 = events
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::SkillListing { text, .. } => Some(self.tokenizer.count(text)),
                _ => None,
            })
            .sum();
        let section_tokens = if stream_tokens > 0 {
            stream_tokens
        } else {
            entry_tokens.iter().sum()
        };
        let shares = split_total(section_tokens, &entry_tokens);
        let announced: HashMap<&str, u32> = announced_skills
            .iter()
            .zip(&shares)
            .map(|(snapshot, tokens)| (snapshot.descriptor.name.as_str(), *tokens))
            .collect();
        let rows = discovered
            .iter()
            .map(|snapshot| {
                let share = announced.get(snapshot.descriptor.name.as_str()).copied();
                ContextSkillEntry {
                    source: snapshot.origin.clone(),
                    name: snapshot.descriptor.name.clone(),
                    tokens: share.unwrap_or(0),
                    announced: share.is_some(),
                }
            })
            .collect();
        SkillCost {
            section_tokens,
            stream_tokens,
            rows,
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
    /// instructions to the system prompt after assembly. The /context pane
    /// measures the session itself rather than reading this, since the pane
    /// reports the conversation cost a turn does not carry.
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
use crate::agent::skill::listing::listing_entry;
use houyicoder_api::skill::SkillSnapshot;
pub use houyicoder_protocol::frontend::context::{
    CategoryBreakdown, ContextBreakdown, ContextFileEntry, ContextSkillEntry, GridCell, build_grid,
};
use houyicoder_protocol::llm::ToolDef;
use prompt::FrozenPrompt;
use prompt::SystemPrompt;
use prompt::cache::PromptHash;
use std::env::current_dir;
use std::env::var;
use std::sync::OnceLock;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[cfg(test)]
mod tests;
