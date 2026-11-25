//! Tests for the lsp subcommand: the parse matrix, the add, remove, and
//! list behavior over real files in throwaway directories, and the report
//! shapes the doctor prints.

use std::env::temp_dir;
use std::fs;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use houyicoder_config::trust::persist_project_trust;

use super::*;
use crate::cli_args::{CliCommand, LspCommand, parse_args};

fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

/// The verbs and their operands parse into the command they name, so the
/// dispatch never sees an argument it did not ask for.
#[test]
fn test_lsp_parse_verbs() {
    let cmd = parse_args(words(&["lsp", "list"])).expect("list parses");
    assert_eq!(
        cmd,
        CliCommand::Lsp {
            action: LspCommand::List
        }
    );
    let cmd = parse_args(words(&["lsp", "doctor"])).expect("doctor parses");
    assert_eq!(
        cmd,
        CliCommand::Lsp {
            action: LspCommand::Doctor
        }
    );
}

/// An add spells the extension mapping normalized (lowercase, leading dot)
/// and keeps the command with its arguments after the separator.
#[test]
fn test_lsp_parse_add() {
    let cmd = parse_args(words(&[
        "lsp",
        "add",
        "rust",
        "--project",
        "--ext",
        ".RS=Rust",
        "--ext",
        "toml=toml",
        "--",
        "rust-analyzer",
        "--log-file",
        "lsp.log",
    ]))
    .expect("add parses");
    assert_eq!(
        cmd,
        CliCommand::Lsp {
            action: LspCommand::Add {
                name: "rust".into(),
                project: true,
                extensions: vec![
                    (".rs".into(), "Rust".into()),
                    (".toml".into(), "toml".into()),
                ],
                command: words(&["rust-analyzer", "--log-file", "lsp.log"]),
            }
        }
    );
}

/// A remove names its entry and its scope.
#[test]
fn test_lsp_parse_remove() {
    let cmd = parse_args(words(&["lsp", "remove", "rust", "--project"])).expect("remove parses");
    assert_eq!(
        cmd,
        CliCommand::Lsp {
            action: LspCommand::Remove {
                name: "rust".into(),
                project: true,
            }
        }
    );
}

/// The malformed shapes are named, not guessed at: a missing verb, a
/// routeless add, an add with no command, a dangling or malformed mapping,
/// an empty name, and a verb nobody defined.
#[test]
fn test_lsp_parse_rejects() {
    let cases: Vec<(Vec<String>, &str)> = vec![
        (words(&["lsp"]), "needs a verb"),
        (
            words(&["lsp", "add", "x", "--", "cmd"]),
            "needs at least one",
        ),
        (
            words(&["lsp", "add", "x", "--ext", ".rs=rust"]),
            "needs a command",
        ),
        (words(&["lsp", "add", "x", "--ext"]), "needs <suffix>"),
        (
            words(&["lsp", "add", "x", "--ext", "rust", "--", "cmd"]),
            "not <suffix>=<language>",
        ),
        (
            words(&["lsp", "add", "x", "--ext", "=rust", "--", "cmd"]),
            "non-empty suffix",
        ),
        (
            words(&["lsp", "add", "", "--ext", ".rs=rust", "--", "cmd"]),
            "needs a server name",
        ),
        (words(&["lsp", "sync"]), "unknown verb"),
        (words(&["lsp", "list", "extra"]), "takes no arguments"),
    ];
    for (args, expected) in cases {
        let error = parse_args(args).expect_err("the malformed shape is rejected");
        assert!(
            error.contains(expected),
            "the error names the problem, saw {error}"
        );
    }
}

/// A throwaway directory holding a settings file and a workspace, so the
/// file-touching behavior never reads or writes a real user's state.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        let root = temp_dir().join(format!("lsp-cli-{}-{tag}-{}", id(), scratch_seq()));
        fs::create_dir_all(root.join("home/.houyicoder")).expect("the home tree is created");
        fs::create_dir_all(root.join("work")).expect("the workspace is created");
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn settings(&self) -> PathBuf {
        self.root.join("home/.houyicoder/settings.json")
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("work")
    }

    fn resolved(&self) -> Vec<ScopedLspServerConfig> {
        resolved(
            &self.settings(),
            &self.workspace(),
            Some(&self.home()),
            None,
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
    }
}

fn scratch_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// An add lands in the scope it names, with the routing table the resolver
/// reads, so the entry a session mounts is the entry the command wrote.
#[test]
fn test_lsp_add_writes_scopes() {
    let scratch = Scratch::new("add");
    add(
        "rust",
        false,
        &[(".rs".into(), "rust".into())],
        &words(&["rust-analyzer"]),
        &scratch.settings(),
        &scratch.workspace(),
    )
    .expect("the user add applies");
    let servers = scratch.resolved();
    let rust = servers.iter().find(|s| s.name == "rust").expect("present");
    assert_eq!(rust.scope, LspConfigScope::User);
    assert_eq!(rust.command, "rust-analyzer");
    assert_eq!(rust.extensions(), vec!["rs".to_string()]);

    add(
        "also",
        true,
        &[(".ts".into(), "typescript".into())],
        &words(&["typescript-language-server", "--stdio"]),
        &scratch.settings(),
        &scratch.workspace(),
    )
    .expect("the project add applies");
    let servers = scratch.resolved();
    let also = servers.iter().find(|s| s.name == "also").expect("present");
    assert_eq!(also.scope, LspConfigScope::Project);
    assert_eq!(also.args, vec!["--stdio".to_string()]);
}

/// A remove takes the entry out of the scope that holds it, and a name
/// nobody holds is an error naming the scope.
#[test]
fn test_lsp_remove_scopes() {
    let scratch = Scratch::new("remove");
    add(
        "rust",
        false,
        &[(".rs".into(), "rust".into())],
        &words(&["rust-analyzer"]),
        &scratch.settings(),
        &scratch.workspace(),
    )
    .expect("the add applies");
    remove("rust", false, &scratch.settings(), &scratch.workspace()).expect("the remove applies");
    assert!(
        scratch
            .resolved()
            .iter()
            .all(|server| server.name != "rust"),
        "the user entry is gone"
    );
    let error = remove("rust", false, &scratch.settings(), &scratch.workspace())
        .expect_err("a second remove is an error");
    assert!(
        error.to_string().contains("no language server named rust"),
        "the error names the entry, saw {error}"
    );
}

/// A remove names a scope: a same-named entry in another scope is not what
/// the command was asked to remove, so it refuses instead of reporting a
/// removal it never made.
#[test]
fn test_lsp_remove_wrong_scope() {
    let scratch = Scratch::new("cross");
    add(
        "also",
        true,
        &[(".ts".into(), "typescript".into())],
        &words(&["tsls"]),
        &scratch.settings(),
        &scratch.workspace(),
    )
    .expect("the project add applies");
    let error = remove("also", false, &scratch.settings(), &scratch.workspace())
        .expect_err("the user scope does not hold it");
    assert!(
        error.to_string().contains("user scope"),
        "the error names the scope that was asked, saw {error}"
    );
    assert!(
        scratch
            .resolved()
            .iter()
            .any(|server| server.name == "also"),
        "the project entry is untouched"
    );
}

/// An add with no command or no extension is refused before anything is
/// written: either would resolve to a server that routes nothing or
/// launches nothing.
#[test]
fn test_lsp_add_rejects_empty() {
    let scratch = Scratch::new("empty");
    let error = add(
        "ghost",
        false,
        &[(".rs".into(), "rust".into())],
        &[],
        &scratch.settings(),
        &scratch.workspace(),
    )
    .expect_err("an empty command is rejected");
    assert!(
        error.to_string().contains("needs a command"),
        "the error names the gap, saw {error}"
    );
    let error = add(
        "ghost",
        false,
        &[],
        &words(&["rust-analyzer"]),
        &scratch.settings(),
        &scratch.workspace(),
    )
    .expect_err("a routeless entry is rejected");
    assert!(
        error.to_string().contains("needs at least one"),
        "the error names the gap, saw {error}"
    );
}

/// The list resolves both scopes and the trust answer: a user entry reads
/// enabled, a project entry in an untrusted workspace reads locked, and the
/// same entry in a trusted workspace reads enabled.
#[test]
fn test_lsp_list_states() {
    let scratch = Scratch::new("list");
    add(
        "rust",
        false,
        &[(".rs".into(), "rust".into())],
        &words(&["rust-analyzer"]),
        &scratch.settings(),
        &scratch.workspace(),
    )
    .expect("the user add applies");
    add(
        "also",
        true,
        &[(".ts".into(), "typescript".into())],
        &words(&["tsls"]),
        &scratch.settings(),
        &scratch.workspace(),
    )
    .expect("the project add applies");
    let servers = scratch.resolved();
    let by_name: std::collections::BTreeMap<_, _> =
        servers.iter().map(|s| (s.name.as_str(), s)).collect();
    assert!(!by_name["rust"].disabled, "the user entry is enabled");
    assert!(
        by_name["also"].disabled,
        "the untrusted project entry is off"
    );
    assert!(by_name["also"].locked, "the trust gate holds it");

    persist_project_trust(&scratch.settings(), &scratch.workspace()).expect("the trust records");
    let servers = scratch.resolved();
    let also = servers.iter().find(|s| s.name == "also").expect("present");
    assert!(!also.disabled, "the trusted project entry is enabled");
    assert!(!also.locked);
}

/// The doctor's report states what it observed: a running server reports
/// its counts and a sub-second span in milliseconds, a failed one reports
/// its reason, its stderr, and the advice, and a starting one reports
/// nothing yet.
#[test]
fn test_lsp_doctor_lines() {
    let mut status = LspServerStatusView {
        name: "rust".into(),
        scope: LspConfigScope::User,
        state: LspServerState::Running,
        locked: false,
        command_line: "rust-analyzer".into(),
        extensions: vec!["rs".into()],
        open_documents: 2,
        diagnostic_count: 1,
        recent_stderr: Vec::new(),
    };
    let line = settled_line(&status, Duration::from_millis(120)).expect("a running server reports");
    assert!(line.contains("running after 120 ms"), "{line}");
    assert!(line.contains("2 documents"), "{line}");
    assert!(
        line.contains("1 diagnostic") && !line.contains("1 diagnostics"),
        "a single count reads singular: {line}"
    );

    status.state = LspServerState::Failed {
        error: "the binary is missing".into(),
    };
    status.recent_stderr = vec!["spawn: not found".into()];
    let line = settled_line(&status, Duration::from_secs(2)).expect("a failed server reports");
    assert!(line.contains("failed: the binary is missing"), "{line}");
    assert!(line.contains("spawn: not found"), "{line}");
    assert!(line.contains("fix: check the command"), "{line}");

    status.state = LspServerState::Indexing {
        message: Some("crates".into()),
    };
    assert!(
        settled_line(&status, Duration::from_secs(1)).is_none(),
        "an indexing server has not settled"
    );
}

/// A sub-second span reads in milliseconds and a longer one in seconds.
#[test]
fn test_report_duration_units() {
    assert_eq!(report_duration(Duration::from_millis(120)), "120 ms");
    assert_eq!(report_duration(Duration::from_millis(1_400)), "1.4 s");
}
