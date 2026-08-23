//! Multi-agent runtime: agent definitions, registry, spawn, bus bridge.
//!
//! The orchestration engine and the model-face agent tool share one source
//! of truth for what a sub-agent is. The trait surface stays minimal here;
//! capability gating, token budget, and the bus bridge land later and plug
//! into the types defined here.

pub mod child_prompt;
pub mod dispatch;
pub mod loader;
pub mod registry;
pub mod spawn;
