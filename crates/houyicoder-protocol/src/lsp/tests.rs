//! Tests for this module.

use super::*;

/// The entry parses from the file shape every frontend spells: camelCase
/// keys, an optional argument list, and defaults for everything the author
/// left out.
#[test]
fn test_entry_parses_with_defaults() {
    let text = r#"{
        "command": "rust-analyzer",
        "extensionToLanguage": {".rs": "rust"}
    }"#;
    let server: ScopedLspServerConfig = serde_json::from_str(text).expect("parses");
    assert_eq!(server.command, "rust-analyzer");
    assert_eq!(server.args, Vec::<String>::new());
    assert_eq!(server.startup_timeout_ms, 10_000);
    assert_eq!(server.shutdown_timeout_ms, 2_000);
    assert!(server.restart_on_crash);
    assert_eq!(server.max_restarts, 3);
    assert!(!server.disabled);
    assert!(!server.locked);
    assert!(server.initialization_options.is_none());
    assert!(server.settings.is_none());
}

/// A round trip keeps every field, and the server's name and scope stay off
/// the wire: the file keys entries by name, and the scope belongs to the
/// file that held the entry, not to the entry itself.
#[test]
fn test_entry_omits_name() {
    let mut servers = BTreeMap::new();
    servers.insert(".rs".to_string(), "rust".to_string());
    let mut server = ScopedLspServerConfig::stdio("rust-analyzer", servers);
    server.name = "rust".into();
    server.scope = LspConfigScope::Project;
    server.args = vec!["--log-file".into()];
    server.locked = true;
    let text = serde_json::to_string(&server).expect("serializes");
    assert!(
        !text.contains("\"name\""),
        "the name is the map key: {text}"
    );
    assert!(
        !text.contains("\"scope\""),
        "the scope is the source: {text}"
    );
    let back: ScopedLspServerConfig = serde_json::from_str(&text).expect("parses");
    assert_eq!(back.command, "rust-analyzer");
    assert_eq!(back.args, vec!["--log-file".to_string()]);
    assert!(back.locked);
    assert_eq!(back.scope, LspConfigScope::User, "scope resets on parse");
}

/// The extensions list drops the leading dot and stays sorted, which is what
/// the list row renders and what the router keys by.
#[test]
fn test_extensions_trim_sorted() {
    let mut servers = BTreeMap::new();
    servers.insert(".rs".to_string(), "rust".to_string());
    servers.insert(".toml".to_string(), "toml".to_string());
    let server = ScopedLspServerConfig::stdio("server", servers);
    assert_eq!(
        server.extensions(),
        vec!["rs".to_string(), "toml".to_string()]
    );
}

/// A payload the on-disk format writes reads faithfully: its timeout
/// spellings land in the fields and its transport parses instead of being
/// ignored and launched as a child.
#[test]
fn test_reference_payload_reads() {
    let text = r#"{
        "command": "rust-analyzer",
        "startupTimeout": 60000,
        "shutdownTimeout": 5000,
        "transport": "socket",
        "restartOnCrash": false,
        "maxRestarts": 1,
        "extensionToLanguage": {".rs": "rust"}
    }"#;
    let server: ScopedLspServerConfig = serde_json::from_str(text).expect("parses");
    assert_eq!(server.startup_timeout_ms, 60_000);
    assert_eq!(server.shutdown_timeout_ms, 5_000);
    assert_eq!(server.transport, LspTransportKind::Socket);
    assert!(!server.restart_on_crash);
    assert_eq!(server.max_restarts, 1);
    let plain: ScopedLspServerConfig =
        serde_json::from_str(r#"{"command": "x", "extensionToLanguage": {".x": "x"}}"#)
            .expect("parses");
    assert_eq!(plain.transport, LspTransportKind::Stdio);
}

/// Every operation parses from its protocol spelling, and the predicates
/// split the position-bearing and call-hierarchy operations correctly.
#[test]
fn test_operations_parse() {
    let all = [
        ("goToDefinition", LspOperation::GoToDefinition),
        ("findReferences", LspOperation::FindReferences),
        ("hover", LspOperation::Hover),
        ("documentSymbol", LspOperation::DocumentSymbol),
        ("workspaceSymbol", LspOperation::WorkspaceSymbol),
        ("goToImplementation", LspOperation::GoToImplementation),
        ("prepareCallHierarchy", LspOperation::PrepareCallHierarchy),
        ("incomingCalls", LspOperation::IncomingCalls),
        ("outgoingCalls", LspOperation::OutgoingCalls),
    ];
    for (wire, operation) in all {
        assert_eq!(LspOperation::parse(wire), Some(operation));
        let text = serde_json::to_string(&operation).expect("serializes");
        assert_eq!(
            text,
            format!("\"{wire}\""),
            "the wire name is the protocol term"
        );
    }
    assert_eq!(LspOperation::parse("gotoDefinition"), None);
    assert!(LspOperation::IncomingCalls.is_call_hierarchy());
    assert!(!LspOperation::Hover.is_call_hierarchy());
    assert!(LspOperation::Hover.needs_position());
    assert!(!LspOperation::DocumentSymbol.needs_position());
    assert!(!LspOperation::WorkspaceSymbol.needs_position());
}

/// A status view survives the wire with its state tag intact, so a surface
/// reads the same lifecycle the manager wrote.
#[test]
fn test_status_round_trips() {
    let view = LspServerStatusView {
        name: "rust".into(),
        scope: LspConfigScope::ClaudePlugin,
        state: LspServerState::Indexing {
            message: Some("indexing 3/10".into()),
        },
        command_line: "rust-analyzer".into(),
        extensions: vec!["rs".into()],
        open_documents: 2,
        diagnostic_count: 1,
        recent_stderr: vec!["a warning".into()],
    };
    let text = serde_json::to_string(&view).expect("serializes");
    let back: LspServerStatusView = serde_json::from_str(&text).expect("parses");
    assert_eq!(back, view);
    assert!(text.contains("\"kind\":\"indexing\""), "{text}");
}
