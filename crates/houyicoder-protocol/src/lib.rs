//! Open protocols.
//!
//! The single architectural boundary that in-process plugin architectures
//! fail to enforce. Everything
//! that is not the host speaks one of these:
//! - frontend: JSON-RPC over stdio/UDS/TCP (commands, streaming events,
//!   diffs) — used by TUI / IDE / web / headless-CI.
//! - llm: the LLM streaming event vocabulary (LlmEvent + Usage) shared by the
//!   provider (emits), the agent loop (consumes), and frontends (renders).
//! - a2a: agent-to-agent protocol (capabilities advertisement, messaging,
//!   pub/sub topics) — substrate for multi-agent orchestration.
//! - extension: tool / resource / hook / skill / WASM-plugin ABI.
//! - mcp: bridge that speaks Model Context Protocol as a subset of
//!   extension, so MCP servers are first-class guests.
//!
//! Capability-scoped; deny-by-default.

#![allow(dead_code)] // crate root re-exports protocol modules consumed by other crates; locally unused

pub mod a2a;
pub mod acp_wire;
pub mod acpx;
pub mod cache_policy;
pub mod capability;
pub mod envelope;
pub mod extension;
pub mod framing;
pub mod frontend;
pub mod handshake;
pub mod llm;
pub mod maybe_undefined;
pub mod mcp;
pub mod tool;
pub mod wire;
