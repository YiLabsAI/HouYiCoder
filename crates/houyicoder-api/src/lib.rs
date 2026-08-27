//! Engine-facing port traits: the contracts the harness declares for what
//! the outside world must provide. Ports holds traits plus the default
//! launcher implementation; the composition root constructs concrete
//! implementations and injects them as trait objects.
//!
//! Allowed dependencies are the foundation crates plus tokio (the default
//! launcher spawns via tokio Command). Port signatures reference foundation
//! types; any signature needing an implementation type forces that type down
//! to the foundation first, or the trait does not enter ports.

pub mod cache_policy;
pub mod cost_model;
pub mod hook_fire;
pub mod launcher;
pub mod live;
pub mod mcp;
pub mod memory;
pub mod progress;
pub mod provider;
pub mod sandbox;
pub mod session;
pub mod skill;
pub mod spawn;
pub mod tool;
