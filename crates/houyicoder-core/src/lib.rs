//! Engine.
//!
//! The deterministic + model-driven heart of the platform. Hosts:
//! - agent runtime (single-agent tool-use loop)
//! - orchestration engine (DAG / pipeline / fan-out / checkpoint / resume / replay)
//! - multi-agent runtime (A2A actors + pub/sub bus + worktree manager)
//! - tool registry + dispatch
//! - provider abstraction (capability-aware, multi-model)
//! - token-budget planner
//! - session / event store (append-only, replayable)
//! - OpenTelemetry observability
//!
//! Control plane (deterministic) is strictly separated from the model plane
//! (LLM calls). Workflows are declarative data interpreted by this crate,
//! never scripts — so no scripting runtime lives in the host.

pub mod agent;
pub mod observability;
pub mod provider;
pub mod snapshot;
pub use houyicoder_context::{
    EventId, PermissionVerdict, PrevHash, SessionId, TurnEvent, TurnEventKind,
};
