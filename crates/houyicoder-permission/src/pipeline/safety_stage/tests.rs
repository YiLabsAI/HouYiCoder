//! The protected-path check must judge the file a write actually lands on,
//! not the spelling the caller supplied. A symlink inside the workspace
//! reaches the version-control directory through a name that carries no
//! marker, so a check that only reads the supplied string misses it. For the
//! host-process file tools there is no second line of defence: they write
//! through the process directly, so the kernel fence never sees the call and
//! cannot refuse it the way it refuses the same write from a shell command.

use crate::Decision;
use crate::decision::{DenySource, Outcome};
use crate::mode::{PermissionMode, ToolRequest};
use crate::{DefaultModeGate, ModeGate};
use houyicoder_api::sandbox::{Containment, Coverage, FenceCapability, SideEffect};
use serde_json::Value;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::id;
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

/// The same fence view with a kernel fence active, which is what makes a
/// write to a fence-denied path impossible rather than merely risky.
struct FencedRoot(PathBuf);

impl Containment for FencedRoot {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![self.0.clone()],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn boundary_root(&self) -> Option<Arc<Path>> {
        Some(Arc::from(self.0.as_path()))
    }
    fn denies_control_writes(&self) -> bool {
        true
    }
}

/// A fence that closes control paths but confines only reads: its path rules
/// cover the session, and a write to a closed control file is not among the
/// things the kernel refuses only because those rules exist.
struct ControlClosedReadsOnly(PathBuf);

impl Containment for ControlClosedReadsOnly {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![self.0.clone()],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn boundary_root(&self) -> Option<Arc<Path>> {
        Some(Arc::from(self.0.as_path()))
    }
    fn capability(&self) -> FenceCapability {
        FenceCapability {
            writes: false,
            reads: true,
            network: false,
        }
    }
    fn denies_control_writes(&self) -> bool {
        true
    }
}

/// A fence that opened the workspace for writing without any rule closing a
/// control path inside it: the shape a writable-roots backend has, where the
/// kernel never refuses such a write and the answer therefore still decides.
struct PermissiveFence(PathBuf);

impl Containment for PermissiveFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![self.0.clone()],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn boundary_root(&self) -> Option<Arc<Path>> {
        Some(Arc::from(self.0.as_path()))
    }
}

fn bash_req(command: &'static str) -> ToolRequest<'static> {
    let v: &'static Value = Box::leak(serde_json::json!({ "command": command }).into());
    ToolRequest {
        tool_name: "bash",
        input: Some(v),
        is_destructive: false,
        is_read_only: false,
        native_requires_approval: false,
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
        let root = temp_dir().join(format!("houyi-protected-{}-{}", id(), tag));
        create_dir_all(root.join(".git/hooks")).expect("git hooks dir");
        create_dir_all(root.join("src")).expect("src dir");
        let root = dunce::canonicalize(&root).expect("canonical workspace");
        #[cfg(unix)]
        symlink(root.join(".git"), root.join("gitlink")).expect("symlink");
        Self(root)
    }

    fn gate(&self) -> DefaultModeGate {
        DefaultModeGate::with_mode(PermissionMode::Auto)
            .with_containment(Arc::new(RootFence(self.0.clone())))
    }

    fn fenced_gate(&self) -> DefaultModeGate {
        DefaultModeGate::with_mode(PermissionMode::Auto)
            .with_containment(Arc::new(FencedRoot(self.0.clone())))
    }

    fn manual_gate(&self) -> DefaultModeGate {
        DefaultModeGate::with_mode(PermissionMode::Manual)
            .with_containment(Arc::new(RootFence(self.0.clone())))
    }

    fn manual_fenced_gate(&self) -> DefaultModeGate {
        DefaultModeGate::with_mode(PermissionMode::Manual)
            .with_containment(Arc::new(FencedRoot(self.0.clone())))
    }

    fn permissive_gate(&self, mode: PermissionMode) -> DefaultModeGate {
        DefaultModeGate::with_mode(mode).with_containment(Arc::new(PermissiveFence(self.0.clone())))
    }

    fn reads_only_control_gate(&self) -> DefaultModeGate {
        DefaultModeGate::with_mode(PermissionMode::Auto)
            .with_containment(Arc::new(ControlClosedReadsOnly(self.0.clone())))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        remove_dir_all(&self.0).ok();
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

/// A command that writes a control file the fence refuses is rejected outright
/// while the fence is active. Asking would collect a yes the kernel then
/// overrules, which is the failure this check exists to remove.
#[test]
fn test_fenced_control_write_denied() {
    let ws = Workspace::new(line!());
    for command in [
        "echo x >> ~/.houyicoder/settings.json",
        "rm ~/.houyicoder/permissions.json",
        "touch .houyicoder/skill-grants.json",
        "mkdir ~/.claude/skills/new-skill",
        "cp evil.sh .git/hooks/pre-commit",
    ] {
        let d = ws.fenced_gate().decide(&bash_req(command));
        assert_eq!(d.outcome(), Outcome::Deny, "denied: {command}");
        match d {
            Decision::Deny(reason) => assert_eq!(reason.source, DenySource::Fence),
            other => panic!("expected a fence deny, got {other:?}"),
        }
    }
}

/// A fence that closes control paths without confining writes cannot make the
/// refusal certain: the write could still land, so the command keeps the
/// question instead of being refused on a promise the kernel does not keep.
#[test]
fn test_control_write_asks_unconfined() {
    let ws = Workspace::new(line!());
    let command = "echo x >> ~/.houyicoder/settings.json";
    assert_eq!(
        ws.fenced_gate().decide(&bash_req(command)).outcome(),
        Outcome::Deny,
        "a writes-confined fence refuses the write outright"
    );
    assert_eq!(
        ws.reads_only_control_gate()
            .decide(&bash_req(command))
            .outcome(),
        Outcome::Ask,
        "without the writes axis the refusal is a promise the fence does not keep"
    );
}

/// A command that only mentions a control path is not writing it. The fence
/// check used to search the whole line, so a commit subject or a string
/// redirected elsewhere was refused as if it were a write to the file.
#[test]
fn test_control_path_mention_asks() {
    let ws = Workspace::new(line!());
    for command in [
        "git commit -m \"fix .houyicoder/settings.json readonly check\"",
        "echo \"ok .claude/settings.json\" > /tmp/report.txt",
    ] {
        let d = ws.fenced_gate().decide(&bash_req(command));
        assert_eq!(d.outcome(), Outcome::Ask, "mention only: {command}");
    }
}

/// A write to a control child the fence does not deny stays the user's answer.
/// The refusal names the fence as its source, so it may only stand where the
/// kernel would refuse the write too.
///
/// The gate judges the command text alone, against a throwaway workspace root:
/// no shell runs and no file is written.
#[test]
fn test_fence_free_child_asks() {
    let ws = Workspace::new(line!());
    for command in [
        "echo x > .claude/hooks/pre-commit",
        "echo x > .agents/settings.json",
    ] {
        let d = ws.fenced_gate().decide(&bash_req(command));
        assert_eq!(d.outcome(), Outcome::Ask, "fence allows it: {command}");
    }
}

/// A fence that grants the workspace without closing a control path inside it
/// cannot back a refusal: the write lands on such a backend, so it stays the
/// user's answer even though the same command is refused under a fence that
/// closes control paths.
#[test]
fn test_permissive_fence_asks_write() {
    let ws = Workspace::new(line!());
    for command in [
        "echo x >> ~/.houyicoder/settings.json",
        "python3 -c \"open('/Users/von/.houyicoder/settings.json','w').write('{}')\"",
    ] {
        let d = ws
            .permissive_gate(PermissionMode::Auto)
            .decide(&bash_req(command));
        assert_eq!(d.outcome(), Outcome::Ask, "kernel allows it: {command}");
    }
}

/// The read exemption rests on the same refusal, so a fence that does not
/// close control paths keeps the question for a read it cannot prove is one.
#[test]
fn test_permissive_fence_asks_read() {
    let ws = Workspace::new(line!());
    for mode in [PermissionMode::Auto, PermissionMode::Manual] {
        for command in [
            "python3 -c \"import json; print(json.load(open('/Users/von/.houyicoder/settings.json')))\"",
            "bash -c \"cat /Users/von/.houyicoder/permissions.json\"",
        ] {
            let d = ws.permissive_gate(mode).decide(&bash_req(command));
            assert_eq!(d.outcome(), Outcome::Ask, "no kernel refusal: {command}");
        }
    }
}

/// Without a fence the same write can land, so the user's answer still
/// decides and the call is asked rather than refused.
#[test]
fn test_unfenced_control_write_asks() {
    let ws = Workspace::new(line!());
    let d = ws
        .gate()
        .decide(&bash_req("echo x >> ~/.houyicoder/settings.json"));
    assert_eq!(d.outcome(), Outcome::Ask, "unfenced: {d:?}");
}

/// A read of a settings file damages nothing and holds no secret, so an
/// attested read-only command passes in every mode. Before this, Manual asked
/// on every plain read the careful mode is the one a user picks, so the same
/// command asked forever there while Auto let it through.
#[test]
fn test_fenced_settings_read_allowed() {
    let ws = Workspace::new(line!());
    for command in [
        "cat ~/.houyicoder/settings.json",
        "stat -f \"settings.json mtime: %Sm\" ~/.houyicoder/settings.json",
        "jq '.count > 0' ~/.houyicoder/settings.json",
        "stat -f \"mtime -> %Sm\" ~/.houyicoder/settings.json",
    ] {
        let d = ws.fenced_gate().decide(&bash_req(command));
        assert_eq!(d.outcome(), Outcome::Allow, "reads only: {command}");
        let d = ws.manual_fenced_gate().decide(&bash_req(command));
        assert_eq!(d.outcome(), Outcome::Allow, "manual reads only: {command}");
    }
}

/// Without a fence the read waiver is withdrawn, so Manual asks for the same
/// read: the fence's read visibility is what the waiver rests on.
#[test]
fn test_manual_unfenced_read_asks() {
    let ws = Workspace::new(line!());
    let d = ws
        .manual_gate()
        .decide(&bash_req("cat ~/.houyicoder/settings.json"));
    assert_eq!(d.outcome(), Outcome::Ask, "unfenced manual read: {d:?}");
}

/// A read of a credential still asks: the file cannot be damaged by a read,
/// but the read leaks the secret.
#[test]
fn test_credential_read_asks() {
    let ws = Workspace::new(line!());
    for command in [
        "cat ~/.houyicoder/.credentials.json",
        "grep key ~/.claude/.credentials.json",
    ] {
        let d = ws.fenced_gate().decide(&bash_req(command));
        assert_eq!(d.outcome(), Outcome::Ask, "leaks a secret: {command}");
    }
}

/// A script that reads the settings file is the same read as cat, so it passes
/// once the fence makes the file unwritable: the read cannot change it, and the
/// kernel already refuses any write.
#[test]
fn test_script_settings_read_passes() {
    let ws = Workspace::new(line!());
    let command = "cd /Users/von/workspace/hicoder && echo \"settings\"; python3 -c \"\nimport json\nd=json.load(open('/Users/von/.houyicoder/settings.json'))\nprint(json.dumps(d.get('model',{}),indent=2))\n\"";
    let d = ws.fenced_gate().decide(&bash_req(command));
    assert_eq!(d.outcome(), Outcome::Allow, "reads only: {d:?}");
}

/// Backing the settings file up reads it and writes a temporary destination,
/// so the fence refusal does not apply. The source operand used to be read as
/// a write, which refused a backup the kernel allows.
#[test]
fn test_settings_backup_not_denied() {
    let ws = Workspace::new(line!());
    let command = "cd /Users/von/workspace/hicoder && cp ~/.houyicoder/settings.json \"$TMPDIR/settings.backup.$(date +%Y%m%d%H%M%S).json\" && ls -la \"$TMPDIR\"/settings.backup.*.json | tail -1";
    let d = ws.fenced_gate().decide(&bash_req(command));
    assert_ne!(d.outcome(), Outcome::Deny, "reads the source: {d:?}");
}

/// A script that assigns the settings path to a name and replaces that name
/// writes the file, so the refusal names the fence instead of approving a
/// command the kernel then fails.
#[test]
fn test_script_settings_write_denied() {
    let ws = Workspace::new(line!());
    let command = "cd /Users/von/workspace/hicoder && cat > \"$TMPDIR/cleanup.py\" <<'PY'\nimport json, os\n\npath = os.path.expanduser(\"~/.houyicoder/settings.json\")\nwith open(path) as f:\n    d = json.load(f)\n\ntmp = path + \".cleanup.tmp\"\nwith open(tmp, \"w\") as f:\n    json.dump(d, f)\n\nos.replace(tmp, path)\nPY\npython3 \"$TMPDIR/cleanup.py\"";
    let d = ws.fenced_gate().decide(&bash_req(command));
    assert_eq!(d.outcome(), Outcome::Deny, "writes the file: {d:?}");
    match d {
        Decision::Deny(reason) => assert_eq!(reason.source, DenySource::Fence),
        other => panic!("expected a fence deny, got {other:?}"),
    }
}

/// The version-control directory is denied only where the profile denies it.
/// A hook write is refused; the configuration file the repository client
/// maintains is not, so that write stays a question.
#[test]
fn test_git_config_write_asks() {
    let ws = Workspace::new(line!());
    let d = ws.fenced_gate().decide(&bash_req("echo x >> .git/config"));
    assert_eq!(d.outcome(), Outcome::Ask, "not fence-denied: {d:?}");
}
