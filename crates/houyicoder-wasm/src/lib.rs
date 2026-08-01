//! Sandboxed plugin runtime.
//!
//! Wasmtime-based in-process sandbox for plugin/hook logic. Host functions
//! (capability-scoped) are exposed to guests; guests can be authored in
//! Rust / Go / AssemblyScript / TypeScript-compiled-to-WASM.
//!
//! Two execution models, one registration:
//! - WASM in-process: cheap, fast, hard-isolated — default for logic.
//! - external MCP server: separate process — for stateful / heavy-dep plugins.
//!
//! Hooks are declarative event subscriptions on the pub/sub bus returning a
//! structured verdict (allow/deny/modify), never imperative callbacks that
//! can crash the host.

#![allow(dead_code)] // crate root re-exports wasm modules pending consumer wiring; locally unused

pub mod host;
pub mod manifest;
pub mod runtime;
