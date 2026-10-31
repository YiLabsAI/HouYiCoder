//! The block-store observability records a session log carries: what one
//! assembly carried for each stored block, and each read of a stored result.
//! These are durable records, so they change with the log's compatibility
//! contract, not with the in-memory store.

use serde::{Deserialize, Serialize};

/// The form an assembly used to carry one stored block to the model. One
/// definition is shared by the writer and the reader, so a new form cannot be
/// counted under another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewForm {
    /// The full stored content, retrieved from the store.
    Materialize,
    /// The inline preview the tool result carried, with no retrieval.
    Summarize,
    /// The pointer alone, with no retrieval.
    Evict,
    /// The stored content could not be read, so the model was told the block
    /// is unreachable rather than absent.
    Unavailable,
    /// A form written by a newer build.
    #[serde(other)]
    Unknown,
}

/// The form one assembly carried for one stored block. Recorded per assembly so a
/// replayed request states what it carried without re-running the assembler's
/// retention decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedModelView {
    /// The address of the stored block the form stands in for.
    pub block_ref: String,
    /// The form the assembly used for this block.
    pub form: ViewForm,
    /// The assembled size of the form, in the unit the containing
    /// event names. It stays unit-agnostic because an assembly may count
    /// bytes, an estimate, or tokens.
    pub size: u64,
}

/// Who issued a read of a stored result. A model read answers a tool call; a
/// UI read pages a result in a pane. A label this build does not know reads
/// back as unknown rather than being counted as either reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadActor {
    Model,
    Ui,
    /// An actor written by a newer build.
    #[serde(other)]
    Unknown,
}

/// Whether a read delivered the bytes it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOutcome {
    /// The read returned a window of the stored result.
    Read,
    /// The read failed; no bytes were delivered.
    Failed,
    /// An outcome written by a newer build.
    #[serde(other)]
    Unknown,
}

/// One read of a stored result. Recorded so a replay counts reads from the log
/// rather than inferring them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedRead {
    /// The result the read addressed, as the catalog names it.
    pub result_id: String,
    pub actor: ReadActor,
    /// First byte of the range the read covered; zero when it delivered
    /// nothing.
    pub start: u64,
    /// How many bytes the read returned.
    pub bytes: u64,
    pub outcome: ReadOutcome,
}
