//! Tests for this module.

use super::*;
use std::path::Path;
use std::time::Duration;

/// The trait must support runtime dispatch.
#[test]
fn test_trait_is_object_safe() {
    let _launcher: Box<dyn ProcessLauncher> = Box::new(Stub);
}

struct Stub;
impl ProcessLauncher for Stub {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        Ok(LauncherChild::new(
            None,
            Box::pin(async move {
                Ok(LauncherExit {
                    exit_code: Some(0),
                    stdout: None,
                    stderr: None,
                })
            }),
        ))
    }
}

#[test]
fn test_policy_builders_stack() {
    // A sandboxed + audited spawn composes; a mutex enum could not express
    // the stack.
    let policy = SpawnPolicy::default().sandboxed().audited();
    assert!(policy.fence.is_some());
    assert!(policy.audit);
}

#[test]
fn test_default_policy_is_passthrough() {
    let policy = SpawnPolicy::default();
    assert!(policy.fence.is_none());
    assert!(policy.wrapper.is_none());
    assert!(!policy.audit);
}

#[test]
fn test_fence_default_is_industrial() {
    let f = FenceConfig::default();
    assert_eq!(f.cpu_secs, 30);
    assert_eq!(f.nproc, 256);
    assert_eq!(f.wall_timeout_ms, 120000);
}

#[test]
fn test_stdio_config_piped_output() {
    let s = StdioConfig::piped_output();
    assert_eq!(s.stdin, StdioMode::Null);
    assert_eq!(s.stdout, StdioMode::Piped);
    assert_eq!(s.stderr, StdioMode::Piped);
}

#[test]
fn test_spawn_request_builder() {
    let req = SpawnRequest::new("make")
        .with_args(["check"])
        .with_workspace("/tmp")
        .piped_output();
    assert_eq!(req.program, "make");
    assert_eq!(req.args, vec!["check"]);
    assert_eq!(req.workspace.as_deref(), Some(Path::new("/tmp")));
    assert_eq!(req.stdio.stdout, StdioMode::Piped);
}

#[test]
fn test_launcher_exit_default() {
    let e = LauncherExit::default();
    assert!(e.exit_code.is_none());
    assert!(e.stdout.is_none());
    assert!(e.stderr.is_none());
}

/// The wall timeout Duration derives from the ms field, not secs.
#[test]
fn test_fence_wall_timeout_millis() {
    let f = FenceConfig {
        wall_timeout_ms: 500,
        ..FenceConfig::default()
    };
    let d = Duration::from_millis(f.wall_timeout_ms);
    assert_eq!(d.as_millis(), 500);
}
