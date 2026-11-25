//! Memory payload types. These cross the port boundary (the MemoryProvider
//! trait in ports references them), so they live in the foundation crate.
//! The trait stays in ports; the concrete impl stays in the memory crate;
//! the types are shared here so neither ports nor the engine depends on the
//! memory impl crate.

use std::error::Error;
use std::fmt;

/// Taxonomy category for a memory entry. Determines the file frontmatter
/// type label and how the entry is grouped in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    /// User-authored guidance or preferences.
    User,
    /// Corrective feedback observed during a session.
    Feedback,
    /// Project-scoped facts: architecture, conventions, decisions.
    Project,
    /// Reference material pulled from external sources.
    Reference,
}

impl MemoryType {
    /// Stable lowercase label used in file frontmatter.
    pub fn as_label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }

    /// Parse a frontmatter label back into a memory type; case-insensitive.
    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }
}

/// Which writer produced this entry. Orthogonal to MemoryType: the
/// type says what kind of knowledge it is, the origin says whose claim
/// it is. Injected by the host at tool-construction time, never accepted
/// from the LLM — a model-provided origin would let a dream self-promote.
/// Every memory has a known origin; unclassified or hand-edited files
/// default to User so they receive protective treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOrigin {
    /// Explicitly authored by the user. Protected from background overwrites
    /// and automatic archival.
    User,
    /// The primary agent's memory_write call.
    PrimaryAgent,
    /// The extractor fork observing the conversation.
    Extractor,
    /// The consolidation dream fork.
    Dream,
}

impl MemoryOrigin {
    /// Stable lowercase label used in file frontmatter.
    pub fn as_label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::PrimaryAgent => "primary_agent",
            Self::Extractor => "extractor",
            Self::Dream => "dream",
        }
    }

    /// Parse a frontmatter label. Any unrecognized or legacy value defaults
    /// to User so unclassified entries receive protective treatment.
    pub fn from_label(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "primary_agent" | "main_agent" => Self::PrimaryAgent,
            "extractor" => Self::Extractor,
            "dream" => Self::Dream,
            _ => Self::User,
        }
    }

    /// Whether this writer runs in the background without direct user action.
    pub fn is_background(self) -> bool {
        matches!(self, Self::Extractor | Self::Dream)
    }
}

/// Physical storage scope a memory lives in. The provider holds an ordered
/// list of roots (user, project, auto); each topic resolves to one scope by
/// its root position. Distinct from MemoryType: scope says WHERE a memory
/// lives, while MemoryType says WHAT kind of memory it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    /// The user-global root: cross-project memories.
    User,
    /// The project root: checked-in, per-project.
    Project,
    /// The auto root: extractor + dream output, auto-extracted from the
    /// session log.
    Auto,
}

impl MemoryScope {
    /// Stable lowercase label used on the wire + in the /memory pane.
    pub fn as_label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Auto => "auto",
        }
    }

    /// Parse a lowercase label back into a scope; case-insensitive.
    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// Map a root index (position in the provider's ordered roots vec) to a
    /// scope. Positional, by the documented multi-root order (user, project,
    /// auto). A single-root provider maps index 0 to User (tests only).
    pub fn for_root_index(idx: usize) -> Self {
        match idx {
            0 => Self::User,
            1 => Self::Project,
            _ => Self::Auto,
        }
    }
}

/// Estimate the token cost of a string without a tokenizer. Latin text runs
/// about four characters per token, while a CJK character costs at least one
/// token on the vocabularies in use, so counting both at four would understate
/// a Chinese entry by the same factor it is dense. Used by MemoryEntry::new
/// and by the index and recall budgets, which must not let a dense entry pass
/// a limit the model pays for.
pub fn tokens_for(s: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in s.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

/// Whether a character is written without word spacing, where one character
/// carries about one token: the CJK ideographs and their extensions, the
/// Japanese kana, and Korean hangul. Shared by the token estimate and the
/// recall tokenizer so the two cannot disagree about what a dense script is.
pub fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2FA1F}'
            | '\u{3040}'..='\u{309F}'
            | '\u{30A0}'..='\u{30FF}'
            | '\u{AC00}'..='\u{D7AF}'
    )
}

/// A single recalled memory unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub key: String,
    pub content: String,
    pub tokens: usize,
    pub memory_type: MemoryType,
    pub description: String,
    pub mtime_secs: u64,
    pub origin: MemoryOrigin,
}

impl MemoryEntry {
    /// Build an entry, computing the token estimate from content length.
    pub fn new(
        key: impl Into<String>,
        content: impl Into<String>,
        memory_type: MemoryType,
    ) -> Self {
        let content = content.into();
        let tokens = tokens_for(&content);
        Self {
            key: key.into(),
            content,
            tokens,
            memory_type,
            description: String::new(),
            mtime_secs: 0,
            origin: MemoryOrigin::User,
        }
    }

    /// Attach the frontmatter description and the file mtime, turning a bare
    /// entry into a fully-populated recall result for the Memory section.
    pub fn with_meta(mut self, description: impl Into<String>, mtime_secs: u64) -> Self {
        self.description = description.into();
        self.mtime_secs = mtime_secs;
        self
    }

    /// Attach the writer provenance. The host calls this at tool
    /// construction (the LLM never provides origin).
    pub fn with_origin(mut self, origin: MemoryOrigin) -> Self {
        self.origin = origin;
        self
    }
}

/// A lightweight listing row for a memory entry — frontmatter metadata only,
/// no body content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySummary {
    pub key: String,
    pub description: String,
    pub memory_type: MemoryType,
    /// Which storage root (user / project / auto) this topic lives in.
    pub scope: MemoryScope,
    pub mtime_secs: u64,
    pub origin: MemoryOrigin,
}

impl MemorySummary {
    pub fn new(
        key: impl Into<String>,
        description: impl Into<String>,
        memory_type: MemoryType,
        scope: MemoryScope,
        mtime_secs: u64,
    ) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
            memory_type,
            scope,
            mtime_secs,
            origin: MemoryOrigin::User,
        }
    }

    /// Attach the writer provenance read from frontmatter.
    pub fn with_origin(mut self, origin: MemoryOrigin) -> Self {
        self.origin = origin;
        self
    }
}

/// One scored candidate row from a recall rank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRankHit {
    pub key: String,
    pub description: String,
    pub memory_type: MemoryType,
    pub scope: MemoryScope,
    pub mtime_secs: u64,
    /// Weighted lexical overlap score across key, description, and body.
    pub score: u32,
}

impl MemoryRankHit {
    pub fn new(
        key: impl Into<String>,
        description: impl Into<String>,
        memory_type: MemoryType,
        scope: MemoryScope,
        mtime_secs: u64,
        score: u32,
    ) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
            memory_type,
            scope,
            mtime_secs,
            score,
        }
    }
}

/// Advisory recall-frequency counters for one memory key, persisted in a
/// per-scope sidecar so the consolidation dream can nominate stale or
/// high-frequency entries. Advisory: a lost or corrupt sidecar is a cold
/// restart (counters re-accumulate), never an invariant violation. The
/// sidecar is written atomically under the write root's lock, but a skipped
/// or lost update is still just a cold restart for those counters.
/// recall_hits increments when a recall surfaces the key; last_access_ts is
/// the last recall time. The gate_violations counter is fed by the
/// PreToolUse gate and stays zero today — the feed is not wired. The field
/// is reserved so the on-disk schema does not change when that wiring lands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryRecallStats {
    pub key: String,
    pub recall_hits: u64,
    pub gate_violations: u64,
    pub occurrences: u64,
    pub last_access_ts: u64,
}

/// Floor days elapsed since mtime, clamped to 0 (future mtime or clock skew).
/// Pure over the two instants so tests are deterministic; the caller supplies
/// now. The age drives the staleness caveat — models reason poorly on raw
/// timestamps, a human-readable age triggers staleness reasoning.
pub fn memory_age_days(mtime_secs: u64, now_secs: u64) -> u64 {
    now_secs.saturating_sub(mtime_secs) / 86_400
}

/// Human-readable age: today / yesterday / N days ago.
pub fn memory_age_label(age_days: u64) -> String {
    match age_days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        d => format!("{d} days ago"),
    }
}

/// Plain-text staleness caveat for memories more than one day old. Empty for
/// fresh (today or yesterday) memories — a warning there is noise. Models
/// assert stale code-state memories as fact (file:line citations to code that
/// has since changed); the caveat makes the staleness visible so the model
/// verifies before asserting.
pub fn memory_freshness_text(age_days: u64) -> String {
    if age_days <= 1 {
        return String::new();
    }
    format!(
        "This memory is {age_days} days old. Memories are point-in-time observations, \
not live state — claims about code behavior or file:line citations may be outdated. \
Verify against current code before asserting as fact."
    )
}

/// Failures a memory provider can report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    /// Underlying storage I/O failure.
    Io,
    /// No entry matched the requested key.
    NotFound,
    /// A path or key failed the security validator.
    InvalidPath(String),
    /// A record on disk was corrupt (unparseable frontmatter or body).
    Corrupt(String),
    /// The entry looked like it carried a credential. The rule id names the
    /// pattern, never the matched text, so the refusal cannot echo the secret.
    Secret(String),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => f.write_str("memory I/O failure"),
            Self::NotFound => f.write_str("memory entry not found"),
            Self::InvalidPath(msg) => write!(f, "invalid memory path: {msg}"),
            Self::Corrupt(msg) => write!(f, "corrupt memory record: {msg}"),
            Self::Secret(rule) => write!(f, "entry looks like a credential: {rule}"),
        }
    }
}

impl Error for MemoryError {}

#[cfg(test)]
mod tests;
