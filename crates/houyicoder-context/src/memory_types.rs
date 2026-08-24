//! Memory payload types. These cross the port boundary (the MemoryProvider
//! trait in ports references them), so they live in the foundation crate.
//! The trait stays in ports; the concrete impl stays in the memory crate;
//! the types are shared here so neither ports nor the engine depends on the
//! memory impl crate.

use std::fmt;

/// Provenance category for a memory entry. Determines the file frontmatter
/// label and how the entry is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    /// User-authored guidance or preferences.
    User,
    /// Corrective feedback observed during a session.
    Feedback,
    /// Project-scoped facts: architecture, conventions, decisions.
    Project,
    /// Reference material pulled from external sources.
    Reference,
}

impl MemorySource {
    /// Stable lowercase label used in file frontmatter.
    pub fn as_label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }

    /// Parse a frontmatter label back into a source; case-insensitive.
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

/// Which writer produced this entry. Orthogonal to MemorySource: the
/// source says what kind of knowledge it is, the origin says whose claim
/// it is. Injected by the host at tool-construction time, never accepted
/// from the LLM — a model-provided origin would let a dream self-promote.
/// Old files with no origin frontmatter parse as Unknown (machine-
/// writable); the Unknown set only ratchets down as new writes carry a
/// real origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryOrigin {
    /// The main agent's save_memory call.
    MainAgent,
    /// The extractor fork observing the conversation.
    Extractor,
    /// The consolidation dream fork.
    Dream,
    /// Pre-origin files, or an unknown writer. Machine-writable.
    #[default]
    Unknown,
}

impl MemoryOrigin {
    /// Stable lowercase label used in file frontmatter.
    pub fn as_label(self) -> &'static str {
        match self {
            Self::MainAgent => "main_agent",
            Self::Extractor => "extractor",
            Self::Dream => "dream",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a frontmatter label; any unrecognized value is Unknown.
    pub fn from_label(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "main_agent" => Self::MainAgent,
            "extractor" => Self::Extractor,
            "dream" => Self::Dream,
            _ => Self::Unknown,
        }
    }
}

/// Physical storage scope a memory lives in. The provider holds an ordered
/// list of roots (user, project, auto); each topic resolves to one scope by
/// its root position. Distinct from MemorySource (the provenance category):
/// scope says WHERE a memory lives (global / per-project / auto-extracted),
/// source says WHAT it is (a user preference / corrective feedback / a project
/// fact / reference material). The two are orthogonal — a user-preference
/// source can live in any scope.
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

/// Estimate the token cost of a string (one token per four characters,
/// rounded up). Used by MemoryEntry::new to budget without re-tokenizing.
pub fn tokens_for(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// A single recalled memory unit.
///
/// key is the stable identifier (file stem or slug). content is the text
/// surfaced to the agent. tokens is the approximate token cost of content
/// (one token per four characters) so recall can budget without
/// re-tokenizing. source is the provenance category. description is the
/// one-line frontmatter hook shown in the Memory section manifest line.
/// mtime_secs is the topic file modification time in seconds since the UNIX
/// epoch, driving the age label and the staleness caveat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub key: String,
    pub content: String,
    pub tokens: usize,
    pub source: MemorySource,
    pub description: String,
    pub mtime_secs: u64,
    pub origin: MemoryOrigin,
}

impl MemoryEntry {
    /// Build an entry, computing the token estimate from content length.
    /// description and mtime default empty until with_meta attaches them;
    /// origin defaults Unknown until with_origin attaches the writer.
    pub fn new(key: impl Into<String>, content: impl Into<String>, source: MemorySource) -> Self {
        let content = content.into();
        let tokens = tokens_for(&content);
        Self {
            key: key.into(),
            content,
            tokens,
            source,
            description: String::new(),
            mtime_secs: 0,
            origin: MemoryOrigin::Unknown,
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

/// A lightweight listing row for a memory entry — the frontmatter only (key,
/// description, source, mtime), no body content. The listing path reads no
/// full bodies so a /memory browse stays cheap regardless of store size. The
/// full body is fetched on demand via MemoryProvider::show_memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySummary {
    pub key: String,
    pub description: String,
    pub source: MemorySource,
    /// Which storage root (user / project / auto) this topic lives in — the
    /// physical scope, orthogonal to the provenance source above. Drives the
    /// /memory pane scope filter (see a project's memories, or the global set).
    pub scope: MemoryScope,
    pub mtime_secs: u64,
    pub origin: MemoryOrigin,
}

impl MemorySummary {
    pub fn new(
        key: impl Into<String>,
        description: impl Into<String>,
        source: MemorySource,
        scope: MemoryScope,
        mtime_secs: u64,
    ) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
            source,
            scope,
            mtime_secs,
            origin: MemoryOrigin::Unknown,
        }
    }

    /// Attach the writer provenance read from frontmatter.
    pub fn with_origin(mut self, origin: MemoryOrigin) -> Self {
        self.origin = origin;
        self
    }
}

/// Advisory recall-frequency counters for one memory key, persisted in a
/// per-scope sidecar so the consolidation dream can nominate stale or
/// high-frequency entries. Advisory: a lost or corrupt sidecar is a cold
/// restart (counters re-accumulate), never an invariant violation — there is
/// no atomic write, no lock, no self-heal. recall_hits increments when a
/// recall surfaces the key; last_access_ts is the last recall time. The
/// gate_violations counter is fed by the PreToolUse gate and stays zero
/// today — the feed is not wired. The field is reserved so the on-disk
/// schema carries it once that feed lands.
/// does not change when that wiring lands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryRecallStats {
    pub key: String,
    pub recall_hits: u64,
    pub gate_violations: u64,
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
    /// The atomic write could not leave the store in a consistent state.
    AtomicityFailed(String),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => f.write_str("memory I/O failure"),
            Self::NotFound => f.write_str("memory entry not found"),
            Self::InvalidPath(msg) => write!(f, "invalid memory path: {msg}"),
            Self::Corrupt(msg) => write!(f, "corrupt memory record: {msg}"),
            Self::AtomicityFailed(msg) => write!(f, "atomic write failed: {msg}"),
        }
    }
}

impl std::error::Error for MemoryError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokens_for_rounds_up() {
        assert_eq!(tokens_for(""), 0);
        assert_eq!(tokens_for("ab"), 1);
        assert_eq!(tokens_for("abcd"), 1);
        assert_eq!(tokens_for("abcde"), 2);
    }

    #[test]
    fn test_entry_new_computes_tokens() {
        let e = MemoryEntry::new("k", "abcd", MemorySource::User);
        assert_eq!(e.tokens, 1);
        let e = MemoryEntry::new("k", "abcdef", MemorySource::Project);
        assert_eq!(e.tokens, 2);
    }

    #[test]
    fn test_source_label_round_trip() {
        for s in [
            MemorySource::User,
            MemorySource::Feedback,
            MemorySource::Project,
            MemorySource::Reference,
        ] {
            assert_eq!(MemorySource::from_label(s.as_label()), Some(s));
        }
        assert!(MemorySource::from_label("unknown").is_none());
    }

    #[test]
    fn test_source_label_case_insensitive() {
        assert_eq!(MemorySource::from_label("USER"), Some(MemorySource::User));
    }

    #[test]
    fn test_age_days_clamps_future() {
        // Same instant -> 0; one day ago -> 1; five days -> 5; future clamps to 0.
        assert_eq!(memory_age_days(1000, 1000), 0);
        assert_eq!(memory_age_days(1000, 1000 + 86_400), 1);
        assert_eq!(memory_age_days(1000, 1000 + 5 * 86_400), 5);
        assert_eq!(memory_age_days(2000, 1000), 0, "future mtime clamps to 0");
    }

    #[test]
    fn test_age_label_branches() {
        assert_eq!(memory_age_label(0), "today");
        assert_eq!(memory_age_label(1), "yesterday");
        assert_eq!(memory_age_label(5), "5 days ago");
        assert_eq!(memory_age_label(47), "47 days ago");
    }

    #[test]
    fn test_freshness_text_threshold() {
        // Fresh (today/yesterday) -> no caveat; older -> caveat naming the age.
        assert!(memory_freshness_text(0).is_empty());
        assert!(memory_freshness_text(1).is_empty());
        let caveat = memory_freshness_text(9);
        assert!(
            caveat.contains("9 days old"),
            "caveat names the age: {caveat}"
        );
        assert!(
            caveat.contains("point-in-time"),
            "caveat flags staleness: {caveat}"
        );
    }

    #[test]
    fn test_with_meta_description_mtime() {
        let e = MemoryEntry::new("k", "body", MemorySource::User).with_meta("a hook line", 1234);
        assert_eq!(e.description, "a hook line");
        assert_eq!(e.mtime_secs, 1234);
        // The bare constructor leaves the meta fields empty for callers that
        // never attach them (e.g. fact extraction that has no file mtime yet).
        let bare = MemoryEntry::new("k", "body", MemorySource::User);
        assert!(bare.description.is_empty());
        assert_eq!(bare.mtime_secs, 0);
    }
}
