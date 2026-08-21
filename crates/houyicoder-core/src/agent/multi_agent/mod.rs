//! Multi-agent runtime: agent definitions, registry, spawn, bus bridge.
//!
//! Lives in L2 core so the orchestration engine and the model-face agent tool
//! share one source of truth for what a sub-agent is. The trait surface stays
//! minimal here; capability gating, token budget, and the bus bridge land in
//! later sprints and plug into the types defined here.

pub mod registry;
