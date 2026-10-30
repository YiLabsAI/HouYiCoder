//! The key names a bounded tool result uses to name the blocks that hold its
//! raw bytes.
//!
//! The writer and the fold read the same object, so both sides name its keys
//! from here. Written twice, a rename on one side would still compile and
//! would leave every capture of the new shape invisible to the catalog.

/// The key a bounded result names its capture under.
pub(crate) const CAPTURE_KEY: &str = "raw_capture";

/// The key inside a capture that holds the record block.
pub(crate) const RECORD_KEY: &str = "record";

/// The key a capture uses for the standard output block, which is also the
/// name that stream answers to in the catalog.
pub(crate) const STDOUT_KEY: &str = "stdout";

/// The key a capture uses for the standard error block, which is also the
/// name that stream answers to in the catalog.
pub(crate) const STDERR_KEY: &str = "stderr";
