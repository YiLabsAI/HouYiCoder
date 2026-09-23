//! What a session's delegated children spent, summed from the durable returns
//! they left in the parent's log.
//!
//! The type and fold live at the session-log port because the append owner
//! maintains the same projection incrementally. Re-exporting them here keeps
//! agent consumers on the multi-agent domain surface.

pub use houyicoder_api::session::{SubagentUsage, aggregate_subagent_usage};

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
