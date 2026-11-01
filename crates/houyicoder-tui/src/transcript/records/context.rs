//! Record types for the /context view: the drill-down rows, suggestion entries,
//! and the full view payload carried by the ContextGrid transcript line.

use houyicoder_protocol::frontend::context::ContextBreakdown;

/// Drill-down rows under the /context grid: per-source memory and per-skill
/// footprints. A memory row is a project memory file, the memory index, or the
/// recalled memories. These rows list in two sections below the grid; the
/// measurement supplies them, so the renderer never makes up rows from a pane
/// cache.
#[derive(Debug, Clone, Default)]
pub struct ContextDrillDown {
    pub memory_files: Vec<ContextFileEntry>,
    pub skills: Vec<ContextSkillEntry>,
}

impl ContextDrillDown {
    /// Read the drill-down rows the breakdown carries. The measurement produces
    /// them, so the frontend renders exactly what the breakdown reported.
    pub fn from_breakdown(breakdown: &ContextBreakdown) -> Self {
        Self {
            memory_files: breakdown
                .memory_files
                .iter()
                .map(|f| ContextFileEntry {
                    label: f.label.clone(),
                    tokens: f.tokens,
                })
                .collect(),
            skills: breakdown
                .skills
                .iter()
                .map(|s| ContextSkillEntry {
                    source: s.source.clone(),
                    name: s.name.clone(),
                    tokens: s.tokens,
                    announced: s.announced,
                })
                .collect(),
        }
    }
}

/// One memory-source row: a project memory file name, the memory index, or the
/// recalled memories, with the tokens it costs.
#[derive(Debug, Clone)]
pub struct ContextFileEntry {
    pub label: String,
    pub tokens: u32,
}

/// One skill row: discovery origin key + name + tokens, with whether the
/// listing announces the skill. A skill the listing does not announce costs
/// the window nothing and shows without a token figure.
#[derive(Debug, Clone)]
pub struct ContextSkillEntry {
    pub source: String,
    pub name: String,
    pub tokens: u32,
    pub announced: bool,
}

/// One suggestion row under the grid: severity glyph + bold title + dim
/// detail, optionally a savings estimate.
#[derive(Debug, Clone)]
pub struct ContextSuggestion {
    pub severity: SuggestionSeverity,
    pub title: String,
    pub detail: String,
    pub savings_tokens: Option<u32>,
}

/// Suggestion severity maps to the glyph + color: warning (yellow) or info
/// (cyan).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionSeverity {
    Warning,
    Info,
}

impl SuggestionSeverity {
    /// Glyph prefix for a suggestion row: warning -> the heavy warning mark,
    /// info -> the circled i.
    pub fn glyph(self) -> char {
        match self {
            Self::Warning => '\u{26A0}',
            Self::Info => '\u{2139}',
        }
    }
}

/// The full /context view payload: the proportional grid + categories, the
/// drill-down rows, and the suggestions. Carried by the ContextGrid transcript
/// line so the inline renderer has everything it needs.
///
/// The measured breakdown is boxed: it is a few hundred bytes of wire DTO, and
/// every transcript row would otherwise pay for it inside the row enum, while
/// the view is read by reference on each render.
#[derive(Debug, Clone, Default)]
pub struct ContextView {
    pub breakdown: Box<ContextBreakdown>,
    pub drill: ContextDrillDown,
    pub suggestions: Vec<ContextSuggestion>,
}
