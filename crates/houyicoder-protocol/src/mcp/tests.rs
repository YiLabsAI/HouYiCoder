//! Tests for this module.

use super::*;

/// The standard config file spells a stdio server with no kind field: the
/// command, its arguments, and nothing else. That file must deserialize as
/// the stdio transport without a rewrite.
#[test]
fn test_stdio_without_kind_parses() {
    let raw = serde_json::json!({
        "command": "codegraph",
        "args": ["serve", "--mcp"],
        "env": {"TOKEN": "abc"},
    });
    let transport: McpTransportConfig = serde_json::from_value(raw).expect("parses");
    assert_eq!(transport.kind, McpTransportKind::Stdio);
    assert_eq!(transport.endpoint, "codegraph");
    assert_eq!(transport.arguments, vec!["serve", "--mcp"]);
    assert_eq!(
        transport.environment.get("TOKEN").map(String::as_str),
        Some("abc")
    );
}

/// The typed spelling carries the kind explicitly, so an HTTP server with
/// headers survives the same parse.
#[test]
fn test_typed_http_parses() {
    let transport: McpTransportConfig = serde_json::from_value(serde_json::json!({
        "type": "http",
        "url": "https://example.invalid/mcp",
        "headers": {"Authorization": "Bearer token"},
    }))
    .expect("parses");
    assert_eq!(transport.kind, McpTransportKind::Http);
    assert_eq!(transport.endpoint, "https://example.invalid/mcp");
    assert_eq!(
        transport.headers.get("Authorization").map(String::as_str),
        Some("Bearer token")
    );
}

/// A body that names only a URL, with no type field, is an HTTP server: the
/// address it carries is what decides the transport. Reading it as a child
/// process would try to launch a URL as a command.
#[test]
fn test_url_only_is_http() {
    let transport: McpTransportConfig = serde_json::from_value(serde_json::json!({
        "url": "https://example.invalid/mcp",
    }))
    .expect("parses");
    assert_eq!(transport.kind, McpTransportKind::Http);
    assert_eq!(transport.endpoint, "https://example.invalid/mcp");
}

/// A headers helper parses under the file's own key and survives a round
/// trip, so a command-supplied header keeps working through a write-back.
#[test]
fn test_headers_helper_parses() {
    let transport: McpTransportConfig = serde_json::from_value(serde_json::json!({
        "type": "http",
        "url": "https://example.invalid/mcp",
        "headersHelper": "token-helper",
    }))
    .expect("parses");
    assert_eq!(transport.headers_helper.as_deref(), Some("token-helper"));
    let text = serde_json::to_string(&transport).expect("serializes");
    let back: McpTransportConfig = serde_json::from_str(&text).expect("re-parses");
    assert_eq!(back, transport);
}

/// The streamable HTTP constructor sets the kind and endpoint and leaves the
/// stdio-only fields empty.
#[test]
fn test_http_constructor_fields() {
    let transport = McpTransportConfig::http("https://example.invalid/mcp");
    assert_eq!(transport.kind, McpTransportKind::Http);
    assert_eq!(transport.endpoint, "https://example.invalid/mcp");
    assert!(transport.arguments.is_empty());
    assert!(transport.environment.is_empty());
    assert!(transport.headers.is_empty());
}

/// One server's fields round-trip: what a caller writes is what a reader
/// sees, with the defaults filled where the source omitted them.
#[test]
fn test_server_round_trips() {
    let server = ScopedMcpServerConfig {
        name: "codegraph".into(),
        scope: McpConfigScope::Project,
        transport: McpTransportConfig::stdio("codegraph", vec!["serve".into()]),
        disabled: false,
        tool_timeout_ms: default_tool_timeout_ms(),
    };
    let text = serde_json::to_string(&server).expect("serializes");
    let back: ScopedMcpServerConfig = serde_json::from_str(&text).expect("parses");
    assert_eq!(back, server);
    assert_eq!(back.tool_timeout_ms, 60_000, "the default is 60s");
}

/// An absent annotations struct means unknown, not safe: every hint reads as
/// None so a server that says nothing stays fail-closed downstream.
#[test]
fn test_annotations_default_unknown() {
    let none: McpToolAnnotations = serde_json::from_value(serde_json::json!({})).expect("parses");
    assert_eq!(none, McpToolAnnotations::default());
    assert_eq!(none.read_only_hint, None);
    assert_eq!(none.destructive_hint, None);
}

/// The state variants carry their details through a wire round trip, so a
/// status surface reads the same failure text the manager wrote.
#[test]
fn test_state_round_trips() {
    for state in [
        McpConnectionState::Connected,
        McpConnectionState::Pending,
        McpConnectionState::Reconnecting { attempt: 2 },
        McpConnectionState::NeedsAuth {
            reason: "401".into(),
        },
        McpConnectionState::Failed {
            error: "exit".into(),
        },
        McpConnectionState::Disabled {
            reason: "untrusted".into(),
        },
    ] {
        let text = serde_json::to_string(&state).expect("serializes");
        let back: McpConnectionState = serde_json::from_str(&text).expect("parses");
        assert_eq!(back, state);
    }
}
