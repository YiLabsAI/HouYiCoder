//! Tests for this module: a session with no configured servers keeps its
//! registry exactly as it was.

use std::env::temp_dir;
use std::fs;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;

use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use houyicoder_api::session::SessionLog;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::{DefaultModeGate, ModeGate};
use houyicoder_session::SessionStore;

use super::*;
use crate::mcp::LIST_RESOURCES_TOOL;
use crate::mcp::READ_RESOURCE_TOOL;

fn temp_home() -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let home = temp_dir().join(format!("houyi-mcp-wiring-{}-{n}", id()));
    drop(fs::create_dir_all(&home));
    home
}

/// No configured server means no runtime and an untouched registry.
#[test]
fn test_unconfigured_keeps_registry() {
    let home = temp_home();
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let (tools, runtime) = start_and_attach(
        ToolRegistry::new(),
        Some(&home),
        None,
        None,
        None,
        &gate,
        log(),
    );
    assert!(runtime.is_none());
    assert!(tools.is_empty());
    drop(fs::remove_dir_all(&home));
}

/// A configured entry attaches the resource tools behind the gate, and the
/// runtime it returns stops with the test.
#[tokio::test]
async fn test_configured_attaches_tools() {
    let home = temp_home();
    let gate: Arc<dyn ModeGate> = Arc::new(DefaultModeGate::new());
    let disabled = r#"[{"name": "quiet", "command": "fake", "disabled": true}]"#;
    let (tools, runtime) = start_and_attach(
        ToolRegistry::new(),
        Some(&home),
        None,
        Some(disabled),
        Some(&Handle::current()),
        &gate,
        log(),
    );
    let runtime = runtime.expect("a configured entry starts a runtime");
    assert!(tools.contains(LIST_RESOURCES_TOOL));
    assert!(tools.contains(READ_RESOURCE_TOOL));
    runtime.manager.shutdown().await;
    drop(fs::remove_dir_all(&home));
}

fn log() -> Arc<dyn SessionLog> {
    Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())))
}
