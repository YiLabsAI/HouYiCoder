//! The protected-path check must judge the file a write actually lands on,
//! not the spelling the caller supplied. A symlink inside the workspace
//! reaches the version-control directory through a name that carries no
//! marker, so a check that only reads the supplied string misses it. For the
//! host-process file tools there is no second line of defence: they write
//! through the process directly, so the kernel fence never sees the call and
//! cannot refuse it the way it refuses the same write from a shell command.

use crate::decision::Outcome;
use crate::mode::{PermissionMode, ToolRequest};
use crate::{DefaultModeGate, ModeGate};
use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A fence view that reports one workspace root and no extra dirs. Only the
/// boundary queries matter here; coverage is irrelevant to the protected-path
/// stage, so it reports no fence.
struct RootFence(PathBuf);

impl Containment for RootFence {
    fn coverage(&self) -> Coverage {
        Coverage::Unfenced
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn boundary_root(&self) -> Option<Arc<Path>> {
        Some(Arc::from(self.0.as_path()))
    }
}

fn write_req(path: &str) -> ToolRequest<'_> {
    let v: &'static Value = Box::leak(serde_json::json!({ "path": path }).into());
    ToolRequest {
        tool_name: "write",
        input: Some(v),
        is_destructive: true,
        is_read_only: false,
        native_requires_approval: false,
    }
}

/// A workspace holding a version-control directory plus a symlink pointing at
/// it. Removed when the guard drops so a failing assertion does not leak it.
struct Workspace(PathBuf);

impl Workspace {
    fn new(tag: u32) -> Self {
        let root =
            std::env::temp_dir().join(format!("houyi-protected-{}-{}", std::process::id(), tag));
        std::fs::create_dir_all(root.join(".git/hooks")).expect("git hooks dir");
        std::fs::create_dir_all(root.join("src")).expect("src dir");
        let root = dunce::canonicalize(&root).expect("canonical workspace");
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join(".git"), root.join("gitlink")).expect("symlink");
        Self(root)
    }

    fn gate(&self) -> DefaultModeGate {
        DefaultModeGate::with_mode(PermissionMode::Auto)
            .with_containment(Arc::new(RootFence(self.0.clone())))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// A write aimed at the hooks directory through a symlink must Ask. The
/// supplied path carries no marker, so only resolving it reveals where the
/// bytes land. Auto mode is the interesting one: the protected-path stage is
/// the sole gate there, and this write plants code the user's next commit
/// runs outside the sandbox.
#[test]
#[cfg(unix)]
fn test_symlinked_git_dir_asks() {
    use crate::Decision;
    let ws = Workspace::new(line!());
    let d = ws.gate().decide(&write_req("gitlink/hooks/pre-commit"));
    assert_eq!(
        d.outcome(),
        Outcome::Ask,
        "a write reaching the version-control dir through a symlink must Ask"
    );
    match d {
        Decision::Ask(reason) => assert_eq!(
            reason.validator, "protected_path",
            "the protected-path stage must be the one that fires"
        ),
        other => panic!("expected an ask from the protected-path stage, got {other:?}"),
    }
}

/// The spelled-out form still Asks. Without this the symlink assertion could
/// pass because every write asks, rather than because the path was resolved.
#[test]
fn test_plain_git_dir_asks() {
    let ws = Workspace::new(line!());
    assert_eq!(
        ws.gate()
            .decide(&write_req(".git/hooks/pre-commit"))
            .outcome(),
        Outcome::Ask,
        "the spelled-out protected path must still Ask"
    );
}

/// An ordinary source file does not Ask, so the resolving step did not turn
/// the stage into a blanket ask for every write.
#[test]
fn test_ordinary_path_allows() {
    let ws = Workspace::new(line!());
    assert_eq!(
        ws.gate().decide(&write_req("src/main.rs")).outcome(),
        Outcome::Allow,
        "an ordinary workspace write must not Ask"
    );
}

/// With no fence attached the stage keeps its supplied-string behaviour: the
/// spelled-out path still Asks. Resolving is an addition, not a replacement,
/// so a gate built without containment loses no coverage.
#[test]
fn test_no_fence_still_asks() {
    assert_eq!(
        DefaultModeGate::with_mode(PermissionMode::Auto)
            .decide(&write_req(".git/hooks/pre-commit"))
            .outcome(),
        Outcome::Ask,
        "the supplied-string check must survive a gate with no fence"
    );
}

/// A read-only tool touching a protected path must pass: the stage guards
/// only writes, and a read cannot damage a protected file. The write form
/// stays guarded; a read-only call over the same protected pattern is the
/// discriminating case — before this gate it Ask'd as if it were a write.
#[test]
fn test_read_only_protected_passes() {
    let v: &'static Value = Box::leak(serde_json::json!({ "pattern": ".git/" }).into());
    let req = ToolRequest {
        tool_name: "glob",
        input: Some(v),
        is_destructive: false,
        is_read_only: true,
        native_requires_approval: false,
    };
    let d = DefaultModeGate::with_mode(PermissionMode::Auto).decide(&req);
    assert!(
        !matches!(d.outcome(), Outcome::Ask),
        "a read-only call on a protected path must not Ask: {d:?}"
    );
    // A tool that does not declare itself read-only still Ask: the gate
    // falls back to guarded for an unknown read claim (fail-closed).
    let unclaimed = ToolRequest {
        tool_name: "glob",
        input: Some(v),
        is_destructive: false,
        is_read_only: false,
        native_requires_approval: false,
    };
    let d = DefaultModeGate::with_mode(PermissionMode::Auto).decide(&unclaimed);
    assert_eq!(
        d.outcome(),
        Outcome::Ask,
        "a tool that does not claim read-only stays guarded"
    );
}

/// A shell command that writes a protected path stays guarded: bash never
/// declares itself read-only, so the write-only gate does not apply and the
/// command Ask's as before.
#[test]
fn test_shell_write_asks() {
    let v: &'static Value =
        Box::leak(serde_json::json!({ "command": "echo x >> ~/.bashrc" }).into());
    let req = ToolRequest {
        tool_name: "bash",
        input: Some(v),
        is_destructive: false,
        is_read_only: false,
        native_requires_approval: false,
    };
    let d = DefaultModeGate::with_mode(PermissionMode::Auto).decide(&req);
    assert_eq!(
        d.outcome(),
        Outcome::Ask,
        "a shell write to a protected path must Ask"
    );
}
