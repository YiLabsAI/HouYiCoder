//! What one context assembly measured.
//!
//! A pass reports the capability inputs it consumed and the sections the
//! request pays for, together with the per-file rows the /context drill-down
//! lists. The builder produces this record and every later reader reads it,
//! so the legend and the detail beneath it describe one measurement.

use houyicoder_api::skill::SkillSnapshot;
use houyicoder_protocol::frontend::context::{
    CategoryBreakdown, ContextBreakdown, ContextFileEntry, ContextSkillEntry, DelegatedContext,
    ToolFootprint, build_grid,
};
use houyicoder_protocol::llm::{InputItem, ToolDef};

/// The capability-derived inputs one assembly measures: the tool schemas the
/// request declares, the frozen memory index the prompt carries, the skills
/// the listing announces, and every skill discovery found. The turn path and
/// the /context measurement build the same set, so a reported breakdown
/// describes the request that would be sent.
///
/// The announced skills are a subset of the discovered ones: the /context
/// rows cover discovery so a skill the listing does not announce still shows,
/// and only the announced part is counted, because only the announced part
/// reaches the window.
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
        let grid = build_grid(&categories, context_window, 80, None);
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
            cas: None,
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
pub(crate) fn split_total(total: u32, parts: &[u32]) -> Vec<u32> {
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
