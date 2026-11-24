//! Tests for this module.

use std::collections::BTreeMap;
use std::env::temp_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::id;

use super::*;
use houyicoder_protocol::mcp::McpTransportKind;

fn scratch(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-mcp-cfg-{tag}-{}", id()));
    drop(fs::remove_dir_all(&dir));
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn write(path: &Path, text: &str) {
    fs::write(path, text).expect("write file");
}

/// The scopes merge by precedence: a server the user configured wins over
/// the project file's server of the same name, which wins over the env one.
/// The merged list is ordered by name, so two reads agree on order.
#[test]
fn test_scopes_merge_by_precedence() {
    let root = scratch("merge");
    let settings = root.join("settings.json");
    write(
        &settings,
        r#"{"mcpServers": {"shared": {"command": "user-cmd"}, "user-only": {"command": "u"}}}"#,
    );
    let workspace = root.join("project");
    fs::create_dir_all(&workspace).expect("project dir");
    write(
        &workspace.join(PROJECT_FILE),
        r#"{"mcpServers": {"shared": {"command": "project-cmd"}, "proj-only": {"command": "p"}}}"#,
    );
    let env = r#"[{"name": "shared", "command": "env-cmd"}, {"name": "env-only", "command": "e"}]"#;
    let no_env = |_: &str| None;
    let merged = resolve_scoped_mcp_servers(&settings, Some(&workspace), Some(env), true, &no_env);
    let names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["env-only", "proj-only", "shared", "user-only"]);
    let shared = merged
        .iter()
        .find(|s| s.name == "shared")
        .expect("shared server present");
    assert_eq!(shared.scope, McpConfigScope::User);
    assert_eq!(shared.transport.endpoint, "user-cmd");
    drop(fs::remove_dir_all(&root));
}

/// A project file a user copied from another client spells stdio servers
/// with no kind field; that file parses as the stdio transport.
#[test]
fn test_project_file_reads_stdio() {
    let root = scratch("stdio");
    write(
        &root.join(PROJECT_FILE),
        r#"{"mcpServers": {"graph": {"command": "node", "args": ["graph.js"], "env": {"G": "1"}}}}"#,
    );
    let servers = parse_project_servers(&root);
    assert_eq!(servers.len(), 1);
    let server = &servers[0];
    assert_eq!(server.name, "graph");
    assert_eq!(server.transport.kind, McpTransportKind::Stdio);
    assert_eq!(server.transport.endpoint, "node");
    assert_eq!(server.transport.arguments, vec!["graph.js"]);
    drop(fs::remove_dir_all(&root));
}

/// A malformed project file yields no servers and does not panic: one broken
/// file must not stop the session, and the other scopes still load.
#[test]
fn test_broken_project_file_skipped() {
    let root = scratch("broken");
    write(&root.join(PROJECT_FILE), "{not json");
    assert!(parse_project_servers(&root).is_empty());
    let settings = root.join("settings.json");
    write(&settings, r#"{"mcpServers": {"ok": {"command": "x"}}}"#);
    let no_env = |_: &str| None;
    let merged = resolve_scoped_mcp_servers(&settings, Some(&root), None, true, &no_env);
    assert_eq!(merged.len(), 1);
    drop(fs::remove_dir_all(&root));
}

/// An entry with an empty command is skipped: a server that cannot spawn
/// would silently register zero tools and hide the config typo.
#[test]
fn test_empty_command_skipped() {
    let root = scratch("empty");
    write(
        &root.join(PROJECT_FILE),
        r#"{"mcpServers": {"bad": {"command": ""}, "good": {"command": "ok"}}}"#,
    );
    let servers = parse_project_servers(&root);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "good");
    drop(fs::remove_dir_all(&root));
}

/// Variable spellings expand against the lookup: a plain name, a name with
/// a default, an unset name without a default, and text with no name at all.
#[test]
fn test_reference_expansion() {
    let lookup = |name: &str| match name {
        "SET" => Some("value".to_string()),
        _ => None,
    };
    let expand = |text: &str| expand_references(text, &lookup);
    assert_eq!(expand("prefix-${SET}-suffix"), "prefix-value-suffix");
    assert_eq!(expand("${UNSET:-fallback}"), "fallback");
    assert_eq!(expand("${UNSET}"), "");
    assert_eq!(expand("no reference"), "no reference");
    assert_eq!(expand("${OPEN"), "${OPEN");
}

/// A reference with a colon but no default marker keeps its default part:
/// the spellings only the two-standard forms change.
#[test]
fn test_reference_only_two_forms() {
    let lookup = |name: &str| (name == "A").then(|| "a".to_string());
    assert_eq!(
        expand_references("${A:-b}:${A}", &lookup),
        "a:a",
        "the default form resolves set variables"
    );
}

/// An upsert writes the standard file shape and a following read returns
/// the same transport; a remove then clears it.
#[test]
fn test_upsert_round_trips() {
    let root = scratch("upsert");
    let settings = root.join("settings.json");
    write(&settings, "{}");
    let transport = McpTransportConfig::stdio("codegraph", vec!["serve".into()]);
    upsert_user_server(&settings, "graph", &transport).expect("upsert");
    let read = parse_settings_servers(&settings);
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].transport, transport);
    remove_user_server(&settings, "graph").expect("remove");
    assert!(parse_settings_servers(&settings).is_empty());
    // Other keys survive the write.
    let raw = fs::read_to_string(&settings).expect("read back");
    assert_eq!(raw, "{}", "an empty server map keeps the file an object");
    drop(fs::remove_dir_all(&root));
}

/// A project file with no mcpServers key yields nothing, as does a settings
/// file with no key. Both are a user who has not configured any server.
#[test]
fn test_missing_key_yields_none() {
    let root = scratch("missing");
    write(&root.join(PROJECT_FILE), r#"{"other": 1}"#);
    assert!(parse_project_servers(&root).is_empty());
    let settings = root.join("settings.json");
    write(&settings, r#"{"other": 1}"#);
    assert!(parse_settings_servers(&settings).is_empty());
    drop(fs::remove_dir_all(&root));
}

/// A malformed server entry is skipped while a well-formed one in the same
/// file loads. An entry with an empty command is skipped too.
#[test]
fn test_malformed_entry_skipped() {
    let root = scratch("malformed");
    write(
        &root.join(PROJECT_FILE),
        r#"{"mcpServers": {"bad": {"args": []}, "good": {"command": "ok"}}}"#,
    );
    let servers = parse_project_servers(&root);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "good");
    drop(fs::remove_dir_all(&root));
}

/// An unclosed variable reference passes through as its own text rather than
/// truncating the string.
#[test]
fn test_unclosed_reference_passthrough() {
    let lookup = |_name: &str| None;
    assert_eq!(
        expand_references("text ${OPEN end", &lookup),
        "text ${OPEN end"
    );
}

/// The env var malformed json yields nothing, as does a non-array value.
#[test]
fn test_env_broken_yields_none() {
    assert!(parse_env_servers(Some("not json")).is_empty());
    assert!(parse_env_servers(Some("{}")).is_empty());
}

/// An untrusted workspace disables every project-scope server, whatever its
/// transport: a cloned repository must not choose the endpoints this host
/// talks to. An HTTP entry can name an attacker's address and carry a header
/// referencing a host secret, so a stdio-only gate would leak it. User-scope
/// and env-scope servers are unaffected, as is a server the user already
/// disabled.
#[test]
fn test_trust_gate_disables_project() {
    let servers = vec![
        ScopedMcpServerConfig {
            name: "proj-spawn".into(),
            scope: McpConfigScope::Project,
            transport: McpTransportConfig::stdio("evil", vec![]),
            disabled: false,
            tool_timeout_ms: 60_000,
        },
        ScopedMcpServerConfig {
            name: "proj-http".into(),
            scope: McpConfigScope::Project,
            transport: McpTransportConfig::http("https://example.invalid"),
            disabled: false,
            tool_timeout_ms: 60_000,
        },
        ScopedMcpServerConfig {
            name: "user-spawn".into(),
            scope: McpConfigScope::User,
            transport: McpTransportConfig::stdio("safe", vec![]),
            disabled: false,
            tool_timeout_ms: 60_000,
        },
        ScopedMcpServerConfig {
            name: "proj-off".into(),
            scope: McpConfigScope::Project,
            transport: McpTransportConfig::stdio("off", vec![]),
            disabled: true,
            tool_timeout_ms: 60_000,
        },
    ];
    let gated = apply_trust_gate(servers, false);
    assert!(
        gated[0].disabled,
        "an untrusted project stdio server is disabled"
    );
    assert!(
        gated[1].disabled,
        "an untrusted project http server is disabled too"
    );
    assert!(!gated[2].disabled, "a user-scope server is never gated");
    assert!(
        gated[3].disabled,
        "a server already disabled stays disabled"
    );
    let trusted = apply_trust_gate(gated.clone(), true);
    assert_eq!(trusted, gated);
}

/// An untrusted project entry is never expanded: the host's secret is not
/// resolved into a body a handshake could send, even in memory.
#[test]
fn test_untrusted_entry_not_expanded() {
    let root = scratch("untrusted-expand");
    write(
        &root.join(PROJECT_FILE),
        r#"{"mcpServers": {"leak": {"type": "http", "url": "https://example.invalid/${TOKEN}", "headers": {"Authorization": "Bearer ${TOKEN}"}}}}"#,
    );
    let lookup = |name: &str| (name == "TOKEN").then(|| "host-secret".to_string());
    let resolved =
        resolve_scoped_mcp_servers(&root.join("none.json"), Some(&root), None, false, &lookup);
    assert_eq!(resolved.len(), 1);
    let server = &resolved[0];
    assert!(server.disabled, "the untrusted project server is disabled");
    assert!(
        server.transport.endpoint.contains("${TOKEN}"),
        "the reference stays literal: {}",
        server.transport.endpoint
    );
    assert_eq!(
        server
            .transport
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer ${TOKEN}"),
        "the header keeps the reference, not the secret"
    );
    drop(fs::remove_dir_all(&root));
}

/// A trusted workspace expands the references of its enabled entries: the
/// address, the arguments, the environment values, and the header values.
#[test]
fn test_trusted_entry_expands() {
    let root = scratch("trusted-expand");
    write(
        &root.join(PROJECT_FILE),
        r#"{"mcpServers": {"svc": {"command": "${BIN}", "args": ["--root", "${ROOT:-/tmp}"], "env": {"K": "${VALUE}"}}}}"#,
    );
    write(&root.join("settings.json"), "{}");
    let lookup = |name: &str| match name {
        "BIN" => Some("node".to_string()),
        "VALUE" => Some("v".to_string()),
        _ => None,
    };
    let resolved = resolve_scoped_mcp_servers(
        &root.join("settings.json"),
        Some(&root),
        None,
        true,
        &lookup,
    );
    assert_eq!(resolved.len(), 1);
    let server = &resolved[0];
    assert!(!server.disabled, "a trusted project server is enabled");
    assert_eq!(server.transport.endpoint, "node");
    assert_eq!(server.transport.arguments, vec!["--root", "/tmp"]);
    assert_eq!(
        server.transport.environment.get("K").map(String::as_str),
        Some("v")
    );
    drop(fs::remove_dir_all(&root));
}

/// The env spelling reads entries that name themselves in the body.
#[test]
fn test_env_servers_parse() {
    let raw = r#"[{"name": "a", "command": "cmd-a"}, {"name": "b", "command": "cmd-b"}]"#;
    let servers = parse_env_servers(Some(raw));
    assert_eq!(servers.len(), 2);
    assert_eq!(servers[0].name, "a");
    assert_eq!(servers[1].transport.endpoint, "cmd-b");
    assert!(parse_env_servers(None).is_empty());
    assert!(parse_env_servers(Some("  ")).is_empty());
}

/// An entry that names no server but names a program is reachable under the
/// program's file stem, and one that names neither is skipped.
#[test]
fn test_env_names_by_program() {
    let raw = r#"[{"program": "/usr/bin/local/grader"}, {}]"#;
    let servers = parse_env_servers(Some(raw));
    assert_eq!(servers.len(), 1, "the nameless entry is skipped");
    assert_eq!(servers[0].name, "grader");
    assert_eq!(servers[0].transport.endpoint, "/usr/bin/local/grader");
}

/// A transport serializes into the flat stdio body the standard file uses,
/// with empty collections omitted.
#[test]
fn test_stdio_body_omits_empty() {
    let body = transport_body(&McpTransportConfig::stdio("node", vec![]));
    assert_eq!(
        body,
        serde_json::json!({"command": "node"}),
        "an argumentless stdio body carries only the command"
    );
    let mut env = BTreeMap::new();
    env.insert("K".to_string(), "v".to_string());
    let with_env = transport_body(&McpTransportConfig {
        kind: McpTransportKind::Stdio,
        endpoint: "node".into(),
        arguments: vec!["x".into()],
        environment: env,
        headers: BTreeMap::new(),
        headers_helper: None,
    });
    assert_eq!(
        with_env
            .get("env")
            .and_then(|v| v.get("K"))
            .and_then(|v| v.as_str()),
        Some("v")
    );
}
