//! Tests for the mcp subcommand: the parse matrix, and the add, remove, and
//! list behavior over real files in throwaway directories.

use std::env::temp_dir;
use std::fs;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

use houyicoder_config::trust::persist_project_trust;

use super::*;
use crate::cli_args::{CliCommand, parse_args};

fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

/// The verbs and their operands parse into the command they name, so the
/// dispatch never sees an argument it did not ask for.
#[test]
fn test_mcp_parse_verbs() {
    let cmd = parse_args(words(&["mcp", "list"])).expect("list parses");
    assert_eq!(
        cmd,
        CliCommand::Mcp {
            action: McpCommand::List
        }
    );
    let cmd = parse_args(words(&["mcp", "doctor"])).expect("doctor parses");
    assert_eq!(
        cmd,
        CliCommand::Mcp {
            action: McpCommand::Doctor
        }
    );
}

/// An add with a command spells the stdio transport: the program first, its
/// arguments after, in the project scope when the flag is present.
#[test]
fn test_mcp_parse_add_command() {
    let cmd = parse_args(words(&[
        "mcp",
        "add",
        "graph",
        "--project",
        "--",
        "node",
        "serve.js",
        "--port",
    ]))
    .expect("add parses");
    assert_eq!(
        cmd,
        CliCommand::Mcp {
            action: McpCommand::Add {
                name: "graph".into(),
                project: true,
                url: None,
                command: words(&["node", "serve.js", "--port"]),
            }
        }
    );
}

/// An add with a URL spells the HTTP transport, in the user scope by
/// default.
#[test]
fn test_mcp_parse_add_url() {
    let cmd = parse_args(words(&[
        "mcp",
        "add",
        "remote",
        "--url",
        "https://example.test/mcp",
    ]))
    .expect("add parses");
    assert_eq!(
        cmd,
        CliCommand::Mcp {
            action: McpCommand::Add {
                name: "remote".into(),
                project: false,
                url: Some("https://example.test/mcp".into()),
                command: Vec::new(),
            }
        }
    );
}

/// A remove names its entry and its scope.
#[test]
fn test_mcp_parse_remove() {
    let cmd = parse_args(words(&["mcp", "remove", "remote", "--project"])).expect("remove parses");
    assert_eq!(
        cmd,
        CliCommand::Mcp {
            action: McpCommand::Remove {
                name: "remote".into(),
                project: true,
            }
        }
    );
}

/// The malformed shapes are named, not guessed at: a missing verb, a
/// transportless add, both transports at once, a dangling --url, a --url
/// whose value is the flag that follows it, an empty name, and a verb
/// nobody defined.
#[test]
fn test_mcp_parse_rejects() {
    let cases: Vec<(Vec<String>, &str)> = vec![
        (words(&["mcp"]), "needs a verb"),
        (words(&["mcp", "add", "x"]), "needs a transport"),
        (
            words(&["mcp", "add", "x", "--url", "u", "--", "cmd"]),
            "one transport",
        ),
        (words(&["mcp", "add", "x", "--url"]), "needs an endpoint"),
        // A following flag is not an endpoint: swallowing it would quietly
        // turn the scope flag into a nonsense URL.
        (
            words(&["mcp", "add", "x", "--url", "--project"]),
            "needs an endpoint",
        ),
        (
            words(&["mcp", "add", "", "--url", "u"]),
            "needs a server name",
        ),
        (words(&["mcp", "sync"]), "unknown verb"),
        (words(&["mcp", "list", "extra"]), "takes no arguments"),
        (words(&["mcp", "add", "--url", "u"]), "needs a server name"),
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
        let root = temp_dir().join(format!("mcp-cli-{}-{tag}-{}", id(), scratch_seq()));
        fs::create_dir_all(root.join("home/.houyicoder")).expect("the home tree is created");
        fs::create_dir_all(root.join("work")).expect("the workspace is created");
        Self { root }
    }

    fn settings(&self) -> PathBuf {
        self.root.join("home/.houyicoder/settings.json")
    }

    fn workspace(&self) -> PathBuf {
        self.root.join("work")
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

/// An add lands in the scope it names, in the file shape the resolver reads,
/// so the entry a session mounts is the entry the command wrote.
#[test]
fn test_add_writes_scopes() {
    let scratch = Scratch::new("add");
    let settings = scratch.settings();
    let workspace = scratch.workspace();
    add(
        "remote",
        false,
        Some("https://example.test/mcp".into()),
        &[],
        &settings,
        &workspace,
    )
    .expect("the user add applies");
    let user = houyicoder_config::parse_settings_servers(&settings);
    assert_eq!(user.len(), 1);
    assert_eq!(user[0].name, "remote");

    add(
        "graph",
        true,
        None,
        &words(&["node", "serve.js"]),
        &settings,
        &workspace,
    )
    .expect("the project add applies");
    let project = houyicoder_config::parse_project_servers(&workspace);
    assert_eq!(project.len(), 1);
    assert_eq!(project[0].name, "graph");
    assert_eq!(project[0].transport.kind, McpTransportKind::Stdio);
}

/// A remove takes the entry out of the scope that holds it, and a name
/// nobody holds is an error naming the scope.
#[test]
fn test_mcp_remove_scopes() {
    let scratch = Scratch::new("remove");
    let settings = scratch.settings();
    let workspace = scratch.workspace();
    add(
        "remote",
        false,
        Some("https://example.test/mcp".into()),
        &[],
        &settings,
        &workspace,
    )
    .expect("the add applies");
    remove("remote", false, &settings, &workspace).expect("the remove applies");
    assert!(
        houyicoder_config::parse_settings_servers(&settings).is_empty(),
        "the user entry is gone"
    );
    let error =
        remove("remote", false, &settings, &workspace).expect_err("a second remove is an error");
    assert!(
        error.to_string().contains("no server named remote"),
        "the error names the entry, saw {error}"
    );
}

/// An add with no program and no endpoint is refused before anything is
/// written: an empty command would resolve to a server that launches
/// nothing.
#[test]
fn test_mcp_add_rejects_empty() {
    let scratch = Scratch::new("empty");
    let error = add(
        "ghost",
        false,
        None,
        &[],
        &scratch.settings(),
        &scratch.workspace(),
    )
    .expect_err("an empty transport is rejected");
    assert!(
        error.to_string().contains("needs a command or an endpoint"),
        "the error names the gap, saw {error}"
    );
}

/// The list reads both scopes and the trust answer: a user entry prints
/// enabled, a project entry in an untrusted workspace prints locked, and a
/// project entry in a trusted one prints enabled. The scope's own file
/// decides, not the flag the entry arrived with.
#[test]
fn test_mcp_list_states() {
    let scratch = Scratch::new("list");
    let settings = scratch.settings();
    let workspace = scratch.workspace();
    add(
        "remote",
        false,
        Some("https://example.test/mcp".into()),
        &[],
        &settings,
        &workspace,
    )
    .expect("the user add applies");
    add(
        "graph",
        true,
        None,
        &words(&["node", "serve.js"]),
        &settings,
        &workspace,
    )
    .expect("the project add applies");
    // The workspace is not in the settings' trust list, so its project entry
    // is locked: the same answer a session would act on.
    let servers = resolved(&settings, &workspace, None);
    let by_name: std::collections::BTreeMap<_, _> =
        servers.iter().map(|s| (s.name.as_str(), s)).collect();
    assert_eq!(by_name["remote"].scope, McpConfigScope::User);
    assert!(!by_name["remote"].disabled, "the user entry is enabled");
    assert!(
        by_name["graph"].disabled,
        "the untrusted project entry is off"
    );
    assert_eq!(by_name["graph"].scope, McpConfigScope::Project);

    // Trust the workspace and the same entry reads enabled.
    persist_project_trust(&settings, &workspace).expect("the trust records");
    let servers = resolved(&settings, &workspace, None);
    let graph = servers.iter().find(|s| s.name == "graph").expect("present");
    assert!(!graph.disabled, "the trusted project entry is enabled");
}

/// The connected report states the roots outcome the connection observed:
/// served when the server asked for the session's directories, not
/// requested when it never did. Either way the counts and the time to the
/// report are there, and a sub-second span prints in milliseconds.
#[test]
fn test_doctor_roots_outcome() {
    let served = connected_line("probe", Duration::from_millis(120), 2, 1, true);
    assert!(
        served.contains("roots served"),
        "a server that asked is reported served: {served}"
    );
    assert!(served.contains("2 tools"), "{served}");
    assert!(served.contains("1 resources"), "{served}");
    assert!(
        served.contains("120 ms"),
        "a short span reads in ms: {served}"
    );

    let unasked = connected_line("probe", Duration::from_millis(1_400), 0, 0, false);
    assert!(
        unasked.contains("roots not requested"),
        "a server that never asked is reported so: {unasked}"
    );
    assert!(
        unasked.contains("1.4 s"),
        "a long span reads in seconds: {unasked}"
    );
}

/// A transport spelling builds the entry it names: a URL is HTTP with no
/// arguments, a command is stdio with the program first.
#[test]
fn test_transport_spelling() {
    let http = build_transport(Some("https://example.test/mcp".into()), &[]);
    assert_eq!(http.kind, McpTransportKind::Http);
    assert_eq!(http.endpoint, "https://example.test/mcp");
    let stdio = build_transport(None, &words(&["node", "serve.js"]));
    assert_eq!(stdio.kind, McpTransportKind::Stdio);
    assert_eq!(stdio.endpoint, "node");
    assert_eq!(stdio.arguments, vec!["serve.js".to_string()]);
}
