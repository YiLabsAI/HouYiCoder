//! Export of the collected substrate: the export port and its failure type.
//!
//! The canonical internal format is JSON; other formats are separate exports
//! so a consumer that wants one does not pay for the others.

use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Write;

use super::log::ObservabilityLog;

/// Error produced by an export sink.
#[derive(Debug)]
pub enum ExportError {
    Io,
    Serialize(String),
}

impl Display for ExportError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io => write!(f, "export io error"),
            Self::Serialize(msg) => write!(f, "export serialize error: {msg}"),
        }
    }
}

impl Error for ExportError {}

/// Serialize the substrate (trajectory, tool stats, cost, errors) into an
/// industry-standard format for analysis or post-training. Multiple sinks
/// serve different consumers; the canonical internal format is JSON.
pub trait ExportSink {
    /// Format identifier ("json", "otel-genai", "hf-messages", etc.).
    fn format_name(&self) -> &str;
    /// Serialize the log contents to the writer.
    fn export(&self, log: &ObservabilityLog, out: &mut dyn Write) -> Result<(), ExportError>;
}
