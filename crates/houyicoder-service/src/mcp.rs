//! The MCP client boundary: external tool servers discovered, connected, and
//! adapted into this host's tool surface. The SDK this module wraps and the
//! transport it speaks stay inside it; every other layer holds the workspace's
//! own types (a tool, a status view, an instruction block).

mod client;
mod stdio_bridge;

pub use client::{ClientEvent, HouyiMcpClientHandler, LaunchedMcpServer, launch_stdio};
pub use stdio_bridge::StderrTail;
