//! The typed paging vocabulary of the result catalog.
//!
//! A command's output outlives the turn that ran it: the bounded result the
//! model reads is a view of bytes the host keeps elsewhere. This module names
//! what a reader asks for (one page of listed results, a bounded window of one
//! stream, a search page, an aggregate) and what it gets back. The catalog
//! implementation lives behind a port; these types are the shared shape every
//! reader and writer agrees on, so a change to paging is visible in one file.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::capture_record::CaptureStopReason;

/// The most entries one list page holds.
///
/// A page is sized for a model read, not for a screen: the caller that shows
/// results can ask again with the cursor it got back. A larger limit is
/// clamped rather than refused, because the caller that passed one still wants
/// a page.
pub const MAX_PAGE_ENTRIES: u32 = 20;

/// The most search hits one page holds.
pub const MAX_SEARCH_HITS: u32 = 10;

/// The name a reader holds for one stored stream.
///
/// The host issues it and validates it against the entries it folded; a reader
/// echoes it back rather than building one. Two results can hold the same
/// bytes (content addressing dedups them) and still be two results, so the
/// name is not the block address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResultId(String);

impl ResultId {
    /// Name a stored stream by the text the host issued for it.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text to hand back to the catalog.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ResultId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which stored stream of a result a reader wants.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "stream")]
pub enum ResultStream {
    /// The standard output a capture kept.
    Stdout,
    /// The standard error a capture kept.
    Stderr,
    /// A block an earlier externalize path stored for one field of the
    /// result. The empty field names a whole output.
    Stored { field: String },
}

/// How the bytes behind a result are to be read.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultEncoding {
    /// The bytes exactly as the pipe delivered them.
    Raw,
    /// A JSON serialization an earlier externalize path wrote. Text inside it
    /// is escaped, so a reader that wants the text asks for it rather than
    /// treating the stored bytes as the text.
    JsonStored,
}

impl ResultEncoding {
    /// Whether the stored bytes are the text a reader sees.
    pub fn is_raw(&self) -> bool {
        matches!(self, Self::Raw)
    }
}

/// How complete the stored bytes are, from the reader's side.
///
/// A capture records complete or truncated with a reason. A block an earlier
/// path stored carries no such evidence, so it reads unknown rather than
/// borrowing the confidence of one that does.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultCompleteness {
    /// Every byte the command printed before it ended was kept.
    Complete,
    /// The kept bytes are a prefix of what the command printed.
    Truncated,
    /// Nothing in the store says how much of the output was kept.
    Unknown,
}

/// A byte range inside one stored stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteSpan {
    /// Offset of the first covered byte.
    pub start: u64,
    /// Number of bytes covered.
    pub len: u64,
}

impl ByteSpan {
    /// The offset one past the last covered byte.
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.len)
    }
}

/// One listed result: a readable stream of one call, with what is known about
/// it without opening its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultEntry {
    /// The name a reader passes back to read or search this entry.
    pub id: ResultId,
    /// The tool whose output this is, absent when the log does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The command the call ran, for a tool that takes one, cut to one bounded
    /// line. Absent when the tool has no command or the log does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The log event that names the stored bytes.
    pub event: String,
    /// The provider-assigned call id, absent for a block an earlier path
    /// stored, which records no call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    /// When the call began, absent when the host clock read before the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_ms: Option<u64>,
    /// The exit status the caller observed, absent when none was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    /// How complete the stored bytes are.
    pub completeness: ResultCompleteness,
    /// What ended the command before its output ended, when one record says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<CaptureStopReason>,
    /// Which stream this entry names.
    pub stream: ResultStream,
    /// How the stored bytes are to be read.
    pub encoding: ResultEncoding,
    /// Stored length in bytes, absent when the store does not report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// The results a list asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultFilter {
    /// Only results of this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Only results whose call began at or after this host time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// Only results whose call began at or before this host time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    /// Only streams holding at least this many bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_bytes: Option<u64>,
}

/// The cursor a page returns for the caller to ask for the next one.
///
/// The host issues it and checks it: it carries the scope, the operation, the
/// newest event the page was taken over, and the position reached, so paging
/// is repeatable within one snapshot and a new result cannot shift a page the
/// caller has not finished reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResultCursor(String);

impl ResultCursor {
    /// Wrap the text the host issued.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text to hand back to the catalog.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A request for one page of listed results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultListQuery {
    /// Which results the page covers.
    #[serde(default)]
    pub filter: ResultFilter,
    /// Where to continue from, absent for the first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ResultCursor>,
    /// How many entries the caller wants, clamped to the page cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One page of listed results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultPage {
    /// The entries this page covers, oldest first.
    pub entries: Vec<ResultEntry>,
    /// Where to continue, absent when nothing newer remains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<ResultCursor>,
    /// Whether the listing answers for every record of the log. False means
    /// the fold has not reached the end yet, or that some records could not
    /// be read, so a result the caller expected may be missing.
    pub complete: bool,
}

/// Which part of a stored stream a read asks for.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "by")]
pub enum ResultRange {
    /// A byte window.
    Bytes {
        /// Offset of the first byte wanted.
        offset: u64,
        /// How many bytes wanted.
        len: u32,
    },
    /// A line window. Finding the line costs a scan from the nearest mark an
    /// earlier read left, so a line further than one read scans is refused
    /// rather than read from the start of the block.
    Lines {
        /// First line wanted, counting from one.
        start: u64,
        /// How many lines wanted.
        count: u32,
    },
}

/// A request for one bounded window of one stored stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultReadQuery {
    /// The stream to read.
    pub id: ResultId,
    /// Which window of it.
    pub range: ResultRange,
    /// Where the read continues, absent for the first read. A byte read resumes
    /// at the offset the cursor names; a line read resumes at the line and stored
    /// offset it names, and the cursor takes precedence over the range's start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ResultCursor>,
}

/// What a read returned.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "content")]
pub enum ResultContent {
    /// Text, from bytes that are valid text once decoded.
    Text {
        /// The decoded window.
        text: String,
    },
    /// Bytes that are not text this reader can show. The count is returned
    /// rather than a lossy decoding, because a replacement character would
    /// read as stored content.
    Opaque {
        /// How many bytes the covered range holds.
        bytes: u64,
    },
}

/// One bounded window of a stored stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultRead {
    /// The stream read.
    pub id: ResultId,
    /// How the stored bytes are to be read.
    pub encoding: ResultEncoding,
    /// The window itself.
    pub content: ResultContent,
    /// The stored bytes this read covered. For text the range is widened to
    /// whole characters at both ends, so a caller resuming at its end never
    /// starts inside one.
    pub covered: ByteSpan,
    /// The stream's stored length, absent when the store does not report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// How complete the stored bytes are.
    pub completeness: ResultCompleteness,
    /// Where to continue, absent when the read reached the end of the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<ResultCursor>,
    /// The first line number the window covers, counting from one, when the
    /// read was by line or the catalog knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

/// How much work one search may spend before returning what it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanBudget {
    /// The most stored bytes one call may scan.
    pub max_bytes: u64,
    /// The most wall time one call may spend.
    pub max_millis: u64,
}

/// A request for one page of search hits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultSearchQuery {
    /// The text to find, matched case-insensitively.
    pub query: String,
    /// Restrict the search to one stream, absent to search every result the
    /// scope holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within: Option<ResultId>,
    /// Where to continue from, absent for the first page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ResultCursor>,
    /// How much work the page may spend, absent for the host default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<ScanBudget>,
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The stream the hit was found in.
    pub id: ResultId,
    /// The stored byte range of the match.
    pub span: ByteSpan,
    /// The line the match begins on, counting from one. Absent when the scan
    /// resumed partway through the stream, because the lines before that point
    /// were not counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// Short text around the match, with ellipsis where it was cut.
    pub snippet: String,
}

/// One page of search hits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultSearchPage {
    /// Hits in the order they were found.
    pub hits: Vec<SearchHit>,
    /// Stored bytes this page scanned.
    pub scanned_bytes: u64,
    /// Whether the search stopped before the scope was covered. A partial
    /// page is not a report that nothing else matches.
    pub partial: bool,
    /// Whether the search covered records the index holds for all of them. A
    /// page that covers only part of the log misses hits in the rest, so an
    /// empty hit list is an answer about these streams, not about everything
    /// the session ever stored.
    pub complete: bool,
    /// Where to continue, absent when the scope was covered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<ResultCursor>,
}

/// A fixed-size aggregate over one session's results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultStats {
    /// Results folded: one per call whose output was kept, plus one per block
    /// an earlier path stored.
    pub results: usize,
    /// Readable streams across those results.
    pub streams: usize,
    /// Streams recorded complete.
    pub complete_streams: usize,
    /// Streams recorded truncated.
    pub truncated_streams: usize,
    /// Streams whose completeness the records do not state, so whether the
    /// kept bytes are whole is unknown. A separate question from
    /// unmeasured_streams, which is about the stored size.
    pub unknown_streams: usize,
    /// Tool results the catalog could not fold: a record that would not read,
    /// or a block the log names that the store does not hold.
    pub corrupt: usize,
    /// Log bytes the fold could not parse, reported so a gap is visible rather
    /// than read as a result that was never stored.
    pub skipped_bytes: u64,
    /// Bytes kept from standard output across the folded results.
    pub stdout_bytes: u64,
    /// Bytes kept from standard error across the folded results.
    pub stderr_bytes: u64,
    /// Stored fields that state no stored size. Only a field can reach this: a
    /// captured stream always states its size. Their bytes are not in the
    /// totals above, so a figure with a non-zero count here is a floor.
    #[serde(default)]
    pub unmeasured_streams: usize,
    /// Earliest recorded call time, absent when none was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_ms: Option<u64>,
    /// Latest recorded call time, absent when none was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_ms: Option<u64>,
    /// The newest event the aggregate covers, absent when nothing folded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
    /// Whether the aggregate covers every record of the log: the fold reached
    /// the end and everything it read was understood. A false value means the
    /// figures are a floor.
    pub complete: bool,
}

#[cfg(test)]
mod tests;
