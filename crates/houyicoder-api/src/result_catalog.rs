//! The result catalog port: one host-owned entry point for the output a
//! command produced above the bound the model reads.
//!
//! A reader passes the session it works in as the scope. The catalog answers
//! only for results that session's log names, so a name issued for another
//! session finds nothing rather than being refused and confirming it exists.

use std::error::Error;
use std::fmt;

use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_protocol::result_catalog::{
    ResultListQuery, ResultPage, ResultRead, ResultReadQuery, ResultSearchPage, ResultSearchQuery,
    ResultStats,
};

/// Why a catalog request was not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultCatalogError {
    /// No result the scope holds answers this name, and no further reading
    /// is done to tell a missing name from one outside the scope.
    NotFound,
    /// A cursor this scope did not issue for this operation, or one whose
    /// snapshot no longer describes the store.
    InvalidCursor(String),
    /// A range that cannot be read as asked: it starts past the end of the
    /// stream, or asks for a line the scan did not reach.
    InvalidRange(String),
    /// The store cannot read a window of the stream, so the read is refused
    /// rather than answered by materializing the whole block.
    UnsupportedRead(String),
    /// The stored bytes do not match their address, or a record beside them
    /// does not read.
    Corrupt(String),
    /// The session log could not be read.
    Store(String),
}

impl fmt::Display for ResultCatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "no such result in this session"),
            Self::InvalidCursor(detail) => write!(f, "cursor not accepted: {detail}"),
            Self::InvalidRange(detail) => write!(f, "range refused: {detail}"),
            Self::UnsupportedRead(detail) => write!(f, "windowed read is not available: {detail}"),
            Self::Corrupt(detail) => write!(f, "stored result is corrupt: {detail}"),
            Self::Store(detail) => write!(f, "session log read failed: {detail}"),
        }
    }
}

impl Error for ResultCatalogError {}

/// The host's view of one session's stored results.
///
/// Every call names the session it works in. The catalog resolves the names it
/// itself issued; a reader never builds one from a block address, so content
/// addressing still dedups storage without becoming a way to reach another
/// session's bytes.
pub trait ResultCatalog: Send + Sync {
    /// One page of results, oldest first.
    fn list(
        &self,
        scope: SessionId,
        query: ResultListQuery,
    ) -> PFut<'_, Result<ResultPage, ResultCatalogError>>;

    /// One page of search hits over the scope's results.
    fn search(
        &self,
        scope: SessionId,
        query: ResultSearchQuery,
    ) -> PFut<'_, Result<ResultSearchPage, ResultCatalogError>>;

    /// One bounded window of one stream.
    fn read(
        &self,
        scope: SessionId,
        query: ResultReadQuery,
    ) -> PFut<'_, Result<ResultRead, ResultCatalogError>>;

    /// The aggregate over everything the catalog folded for the scope.
    fn stats(&self, scope: SessionId) -> PFut<'_, Result<ResultStats, ResultCatalogError>>;
}

#[cfg(test)]
mod tests;
