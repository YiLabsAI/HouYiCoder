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

pub(crate) mod budget;
pub(crate) mod cache_liveness;
pub(crate) mod cas_views;
pub(crate) mod conversation;
pub(crate) mod measurement;
pub(crate) mod prompt;
pub(crate) mod recall;
pub(crate) mod recap;
pub(crate) mod retention;
pub(crate) mod selection;
pub(crate) mod shell_history;
pub(crate) mod turn_group;

use std::collections::HashMap;
use std::env::{current_dir, var};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::agent::context::conversation::ConversationCost;
use crate::agent::context::measurement::split_total;
use crate::agent::context::retention::AssemblyScope;
use crate::agent::context::turn_group::AssemblyMode;
use crate::agent::multi_agent::registry::{AgentDefinition, agent_directory_section};
use crate::agent::skill::listing::listing_entry;
use houyicoder_api::skill::SkillSnapshot;
use houyicoder_context::{CheckpointManifest, SessionEvent, SessionLogEntry};
use houyicoder_protocol::llm::{AssistantToolCall, InputItem};
use prompt::cache::PromptHash;
use prompt::{FrozenPrompt, SystemPrompt};

pub use houyicoder_protocol::frontend::context::{
    CategoryBreakdown, ContextBreakdown, ContextFileEntry, ContextSkillEntry, GridCell, build_grid,
};
pub(crate) use measurement::memory_entry_label;
pub use measurement::{AssembledContext, AssemblyInput, ContextMeasurement, Section, SectionKind};
pub(crate) use recall::{MEMORY_RECALL_BUDGET, render_recall_text};

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
        if text.len() > 8_192 {
            return (text.len() as u32).div_ceil(4);
        }
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
    /// The assembly counter a retention policy scopes a batch decision to.
    assembly_seq: AtomicU64,
    /// The wall-clock start of the session epoch (the last clear boundary, or
    /// the first event when the session never cleared). The date line derives
    /// from it so a reopened session reproduces the prompt bytes its previous
    /// process sent instead of stamping the reopen time. Zero means unset: a
    /// fresh session uses its first assembly's clock.
    epoch_start_ms: AtomicU64,
    /// The active model's context window, in tokens. The runner updates it on a
    /// model switch so the per-turn tool-result budget tracks the window rather
    /// than a fixed constant. Zero means unset; the budget then keeps its cap.
    model_window: AtomicU32,
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
            assembly_seq: AtomicU64::new(0),
            epoch_start_ms: AtomicU64::new(0),
            model_window: AtomicU32::new(0),
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
            AssemblyMode::default(),
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
            AssemblyMode::default(),
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
        continuation: bool,
    ) -> AssembledContext {
        let assembled = self.assemble(
            events,
            manifest,
            backend,
            input,
            ConversationCost::SectionsOnly,
            AssemblyMode {
                mutating: true,
                continuation,
            },
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
        mode: AssemblyMode,
    ) -> AssembledContext {
        let filtered = match manifest {
            Some(manifest) => selection::apply_manifest(events, manifest, backend),
            None => events.to_vec(),
        };
        let messages = self.assemble_messages(&filtered, backend, mode);
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
        mode: AssemblyMode,
    ) -> Vec<InputItem> {
        let policy = self
            .retention_policy
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        let Some(policy) = policy else {
            return turn_group::assemble_model_input(events, backend);
        };
        // Set the per-turn tool-result budget before materialize runs, so the
        // policy gates on a window-proportional token ceiling rather than a
        // fixed constant. No-op for non-budgeted policies.
        self.push_turn_budget();
        let assembly_id = if mode.mutating && !mode.continuation {
            self.assembly_seq.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            self.assembly_seq.load(Ordering::Relaxed)
        };
        turn_group::assemble_model_input_with(
            events,
            backend,
            &*policy,
            AssemblyScope {
                id: assembly_id,
                mutating: mode.mutating,
            },
        )
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

#[cfg(test)]
mod tests;
