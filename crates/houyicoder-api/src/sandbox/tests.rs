//! Tests for this module.

use super::*;

/// The trait must support runtime dispatch.
#[test]
fn test_trait_is_object_safe() {
    let _session: Box<dyn SandboxSession> = Box::new(Stub);
}

/// Default working-dir impls: a backend that does not override them
/// returns Unsupported on add, no-ops on remove, and an empty list.
/// Backends with a mutable fence override all three.
#[test]
fn test_default_working_dir_unsupported() {
    let s = Stub;
    assert!(matches!(
        s.add_working_dir("/tmp"),
        Err(SandboxError::Unsupported(_))
    ));
    assert!(matches!(
        s.add_reading_dir("/tmp"),
        Err(SandboxError::Unsupported(_))
    ));
    s.remove_working_dir("/tmp"); // no-op, no panic
    assert!(s.working_dirs().is_empty());
    assert!(s.reading_dirs().is_empty());
}

/// Default mach-services no-op: set/clear do not panic.
#[test]
fn test_default_mach_services_noop() {
    let s = Stub;
    s.set_extra_mach_services(&["test.dummy.xpc".to_string()]);
    s.clear_extra_mach_services();
}

struct Stub;
impl SandboxSession for Stub {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async move {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn read_file(&self, _path: &str, _max_bytes: usize) -> PFut<'_, Result<Vec<u8>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn write_file(&self, _path: &str, _content: Vec<u8>) -> PFut<'_, Result<(), SandboxError>> {
        Box::pin(async move { Ok(()) })
    }
    fn list_dir(&self, _path: &str) -> PFut<'_, Result<Vec<DirEntry>, SandboxError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn path_exists(&self, _path: &str) -> PFut<'_, Result<bool, SandboxError>> {
        Box::pin(async move { Ok(false) })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(std::path::PathBuf::from("/"))
    }
}

/// Enforced is the only status that is not unfenced; the other three
/// all carry a user-visible notice. The notice is the composition root's
/// signal to queue a startup system line.
#[test]
fn test_fence_status_unfenced_notice() {
    assert!(!FenceStatus::Enforced.is_unfenced(), "Enforced is fenced");
    assert!(FenceStatus::Enforced.unfenced_notice().is_none());
    for status in [
        FenceStatus::NotEnforced,
        FenceStatus::Failed("landlock error".into()),
        FenceStatus::Unavailable,
    ] {
        assert!(status.is_unfenced(), "{status:?} is unfenced");
        let notice = status.unfenced_notice().expect("unfenced has a notice");
        assert!(
            notice.contains("unfenced"),
            "notice mentions unfenced: {notice}"
        );
    }
    // The Failed variant carries the error detail.
    assert!(
        FenceStatus::Failed("landlock error".into())
            .unfenced_notice()
            .unwrap()
            .contains("landlock error"),
        "Failed notice carries the error"
    );
}

/// The default fence_status (for backends that do not override) is
/// Unavailable — the stub backend has no kernel fence.
#[test]
fn test_fence_status_default_unavailable() {
    let stub = Stub;
    assert_eq!(
        stub.fence_status(),
        FenceStatus::Unavailable,
        "default is Unavailable for backends without a real fence"
    );
}

mod side_effect;

mod containment_contract;
mod default;
