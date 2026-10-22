//! Command-shell selection and the note appended to a refused command.

use super::super::command::PREFERRED_SHELLS;
use super::super::command::annotate_fence_refusal;
use super::super::command::command_shell;
use super::super::command::first_present;
use super::super::command::login_shell;
use houyicoder_context::ExecResult;
use std::path::Path;
use std::path::PathBuf;

fn result(stderr: &str, exit_code: Option<i32>) -> ExecResult {
    ExecResult {
        stdout: String::new(),
        stderr: stderr.to_string(),
        exit_code,
    }
}

/// The session runs bash, because the model writes bash. The choice falls back
/// to the login shell only on a host with no bash at all.
#[test]
fn test_command_shell_is_bash() {
    let shell = command_shell();
    let any_bash_present = PREFERRED_SHELLS.iter().any(|p| Path::new(p).is_file());
    if any_bash_present {
        assert!(
            PREFERRED_SHELLS.contains(&shell.to_str().unwrap_or("")),
            "a bash is present, so the session runs one: {}",
            shell.display()
        );
    }
    assert!(
        shell.is_absolute(),
        "the shell is named by absolute path: {}",
        shell.display()
    );
}

/// A fence refusal names a path and nothing else, so the note states where the
/// session may write. It is appended only to a refusal: a command that failed
/// for its own reasons keeps its own stderr.
#[test]
fn test_fence_refusal_note() {
    let workspace = Path::new("/work/repo");
    let tmpdir = Path::new("/private/var/folders/T/hy-tmp-1");

    let mut refused = result("bash: /etc/hosts: Operation not permitted\n", Some(1));
    annotate_fence_refusal(&mut refused, workspace, tmpdir);
    assert!(
        refused.stderr.contains("Writable roots: /work/repo"),
        "the note names the writable workspace: {}",
        refused.stderr
    );
    assert!(
        refused
            .stderr
            .contains("TMPDIR=/private/var/folders/T/hy-tmp-1"),
        "the note names the session temp dir: {}",
        refused.stderr
    );

    let mut own_failure = result("cargo: error: could not compile\n", Some(101));
    let mut bare_refusal = result("chmod: Operation not permitted\n", Some(1));
    annotate_fence_refusal(&mut own_failure, workspace, tmpdir);
    assert_eq!(
        own_failure.stderr, "cargo: error: could not compile\n",
        "a command's own failure keeps its stderr"
    );

    annotate_fence_refusal(&mut bare_refusal, workspace, tmpdir);
    assert_eq!(
        bare_refusal.stderr, "chmod: Operation not permitted\n",
        "a refusal that names no path is the program's own, not the fence's"
    );

    let mut ok = result("", Some(0));
    annotate_fence_refusal(&mut ok, workspace, tmpdir);
    assert!(ok.stderr.is_empty(), "a successful command carries no note");
}

/// The candidate list is tried in order and the first hit wins, so a package
/// manager build is used when it exists and the system bash when it does not.
#[test]
fn test_first_present_order() {
    let candidates = ["/opt/homebrew/bin/bash", "/bin/bash"];
    let homebrew = |p: &Path| p == Path::new("/opt/homebrew/bin/bash");
    assert_eq!(
        first_present(&candidates, homebrew),
        Some(PathBuf::from("/opt/homebrew/bin/bash"))
    );
    let system_only = |p: &Path| p == Path::new("/bin/bash");
    assert_eq!(
        first_present(&candidates, system_only),
        Some(PathBuf::from("/bin/bash"))
    );
    assert_eq!(first_present(&candidates, |_| false), None);
}

/// The host's login shell is the fallback when no bash is installed. It is read
/// from the environment, so the host running the tests always has one.
#[test]
fn test_login_shell_present() {
    let shell = login_shell();
    assert!(
        shell.is_some(),
        "a test host advertises SHELL; the fallback path is what this covers"
    );
}
