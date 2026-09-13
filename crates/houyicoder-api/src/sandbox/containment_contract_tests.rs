use super::{Containment, Coverage, SideEffect, path_args_for_boundary};
use std::path::PathBuf;

struct StubFence;

impl Containment for StubFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![PathBuf::from("/ws")],
        }
    }

    fn would_block(&self, effect: SideEffect) -> Option<String> {
        match effect {
            SideEffect::Network => Some("egress is contained".into()),
            _ => None,
        }
    }
}

fn assert_dyn_safe(_: &dyn Containment) {}

#[test]
fn test_stub_reports_fenced_coverage() {
    let fence = StubFence;
    assert!(matches!(fence.coverage(), Coverage::Fenced { .. }));
}

#[test]
fn test_boundary_defaults_none_empty() {
    let fence = StubFence;
    assert!(fence.boundary_root().is_none());
    assert!(fence.boundary_dirs().is_empty());
}

#[test]
fn test_stub_blocks_network_only() {
    let fence = StubFence;
    assert!(fence.would_block(SideEffect::Network).is_some());
    assert!(fence.would_block(SideEffect::None).is_none());
}

#[test]
fn test_stub_is_dyn_safe() {
    assert_dyn_safe(&StubFence);
}

#[test]
fn test_file_tools_expose_path() {
    let input = serde_json::json!({"path": "/outside/settings.json"});
    for tool in ["write", "edit", "multiedit"] {
        assert_eq!(
            path_args_for_boundary(tool, Some(&input)),
            vec!["/outside/settings.json"],
            "{tool} must expose its path to the approval boundary"
        );
    }
    assert!(
        path_args_for_boundary("read", Some(&input)).is_empty(),
        "a read approval must not widen a directory's write fence"
    );
}
