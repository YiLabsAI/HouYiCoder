//! Multi-agent runtime: agent definitions, registry, spawn, bus bridge.
//!
//! The orchestration engine and the model-face agent tool share one source
//! of truth for what a sub-agent is. The trait surface stays minimal here;
//! capability gating, token budget, and the bus bridge land later and plug
//! into the types defined here.

pub mod acl;
pub mod budget;
pub mod bus_types;
pub mod capability;
pub mod child_prompt;
pub mod concurrency_gate;
pub mod dispatch;
pub mod ending;
pub mod loader;
pub mod registry;
pub mod spawn;
pub mod status_publisher;
pub mod usage;

pub use spawn::{
    BoundaryOutcome, ChildHandle, ChildLedger, CutReason, OrphanedChild, PartialTranscript,
    RecordOwed, RunnerInheritance, SpawnError, SpawnRequest, completion_notice_pending,
    logged_usage, orphaned_summary, record_orphaned_child, record_subagent_return, spawn_child,
};
pub use status_publisher::{ChildStatusPublisher, recorded_cut, timed_out_summary};
pub use usage::{SubagentUsage, aggregate_subagent_usage};
