//! What a session's stored blocks cost and how its assemblies carried them,
//! folded from the durable records the session wrote. A reader of the
//! trajectory summary consumes these figures; the fold that produces them
//! lives in the session crate, which depends on this crate rather than the
//! other way round.

/// The unit the size figure an assembly's forms make up counts in, as the policy that
/// produced it reported. Unknown when the log names a unit this build does not
/// know, so a reader does not print a byte count as a token count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CasSizeUnit {
    /// The log names no unit this build knows.
    #[default]
    Unknown,
    /// The figure counts tokens the assembly's tokenizer measured.
    Tokens,
    /// The figure counts the bytes of the carried forms.
    Bytes,
}

/// What one assembly carried from the block store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CasAssemblySummary {
    pub assembly_id: u64,
    /// Blocks carried with their stored content.
    pub full: usize,
    /// Blocks carried as the inline preview their marker carries.
    pub preview: usize,
    /// Blocks carried as the address alone.
    pub pointer: usize,
    /// Blocks whose stored content could not be read.
    pub unavailable: usize,
    /// Blocks whose form this build does not know.
    pub unclassified: usize,
    /// The assembled size of every form, in the unit below.
    pub size: u64,
    pub unit: CasSizeUnit,
}

/// What the session's stored blocks cost and how they were carried.
///
/// The figures come from the records the session wrote, never from the
/// in-memory state of a running turn, so a read after a restart reports the
/// same numbers. A count the log does not carry stays absent or is reported as
/// unknown; a capture that predates the size report does not become a capture
/// of zero bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CasUsageSummary {
    /// Results the session stored, counted once per result whose capture
    /// published its record block. None on a log written before the capture
    /// record existed, where a zero would claim the session stored nothing.
    pub results: Option<usize>,
    /// Distinct blocks the session's results address, including ones written
    /// before capture records existed. Every tool result that carries a block
    /// address is evidence of one block, so this count needs no extra record.
    pub blocks: usize,
    /// Raw bytes the session captured, when at least one capture reported a
    /// size. None when none did, so an old session does not report zero.
    pub raw_bytes: Option<u64>,
    /// Results whose capture reported no size, so raw_bytes is a floor. None
    /// when the result count itself is unknown.
    pub unmeasured_results: Option<usize>,
    /// Results whose stored copy failed to land. None when the result count
    /// itself is unknown.
    pub save_failed: Option<usize>,
    /// Results whose capture stopped the command before its output ended.
    /// None when the result count itself is unknown.
    pub truncated: Option<usize>,
    /// What the newest assembly carried, when the log states one.
    pub assembly: Option<CasAssemblySummary>,
    /// Reads of stored results the model made that delivered bytes.
    pub model_reads: usize,
    /// Reads of stored results a pane answered that delivered bytes. None until
    /// a record names the pane as the reader, so a session whose terminal has
    /// no read path yet does not report a measured zero.
    pub ui_reads: Option<usize>,
    /// Reads that delivered nothing.
    pub failed_reads: usize,
    /// Reads whose actor or outcome this build does not know.
    pub unknown_reads: usize,
    /// Whether the log carries a read record, so a reader can tell a session
    /// that read nothing from one whose reads were not recorded.
    pub reads_recorded: bool,
    /// Whether the build that wrote the log records block-store observability
    /// at all. False on an older log, where the model view is unknown.
    pub tracked: bool,
}
