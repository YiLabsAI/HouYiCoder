//! Tests for this module: what the two tools render and what they refuse,
//! with a manager that can never launch a server.

use std::path::Path;
use std::sync::Arc;

use houyicoder_api::tool::ToolCtx;
use serde_json::Value;
use serde_json::json;

use super::*;
use crate::mcp::McpConnections;
use crate::mcp::manager::McpTimings;
use crate::mcp::manager::tests::NoSpawn;

fn manager() -> Arc<McpConnections> {
    Arc::new(McpConnections::new(
        Arc::new(NoSpawn),
        Arc::from(Path::new("/work")),
        McpTimings::default(),
    ))
}

fn resource(uri: &str, name: &str) -> Resource {
    serde_json::from_value(json!({"uri": uri, "name": name})).expect("a resource")
}

/// A listing names each resource with its server, carries the optional
/// fields a server supplied, and omits the ones it did not.
#[test]
fn test_listing_renders() {
    let described = serde_json::from_value(json!({
        "uri": "file:///c",
        "name": "third",
        "description": "the third one",
        "mimeType": "text/markdown"
    }))
    .expect("a described resource");
    let resources = vec![
        ("alpha".to_string(), resource("file:///a", "first")),
        ("beta".to_string(), described),
    ];
    let rendered = render_resources(&resources);
    assert_eq!(rendered.as_array().map(Vec::len), Some(2));
    assert_eq!(rendered[0]["server"], "alpha");
    assert_eq!(rendered[0]["uri"], "file:///a");
    assert!(rendered[0].get("description").is_none());
    assert_eq!(rendered[1]["description"], "the third one");
    assert_eq!(rendered[1]["mimeType"], "text/markdown");
}

/// Text blocks keep their address and body; a binary block is named by size
/// rather than decoded.
#[test]
fn test_contents_render() {
    let text: ResourceContents =
        serde_json::from_value(json!({"uri": "file:///a", "text": "hello"}))
            .expect("text contents");
    let blob: ResourceContents =
        serde_json::from_value(json!({"uri": "file:///b", "blob": "aGVsbG8="}))
            .expect("blob contents");
    let rendered = render_contents(&[text, blob]);
    assert!(rendered.contains("# file:///a"), "{rendered}");
    assert!(rendered.contains("hello"), "{rendered}");
    assert!(rendered.contains("binary resource file:///b"), "{rendered}");
}

/// The tools declare themselves read-only and refuse an input that is not an
/// object before reaching any server.
#[tokio::test]
async fn test_tools_refuse_bad_input() {
    let list = ListMcpResourcesTool::new(manager(), None);
    assert_eq!(list.name(), LIST_RESOURCES_TOOL);
    assert!(!list.description().is_empty());
    assert_eq!(list.input_schema()["type"], "object");
    assert!(list.is_read_only() && !list.is_destructive());
    let empty = list
        .execute(ToolCtx::new("c1"), Value::Null)
        .await
        .expect("a null input lists everything");
    assert_eq!(empty.as_array().map(Vec::len), Some(0));
    let error = list
        .execute(ToolCtx::new("c1"), json!("plain"))
        .await
        .expect_err("a string is not a listing request");
    assert!(error.to_string().contains("a string"), "{error}");

    let read = ReadMcpResourceTool::new(manager(), None);
    assert_eq!(read.name(), READ_RESOURCE_TOOL);
    assert!(!read.description().is_empty());
    assert_eq!(read.input_schema()["required"][0], "server");
    assert!(read.is_read_only() && !read.is_destructive());
    let error = read
        .execute(ToolCtx::new("c1"), json!({}))
        .await
        .expect_err("an empty request names no server");
    assert!(error.to_string().contains("server"), "{error}");
    let error = read
        .execute(ToolCtx::new("c1"), json!(true))
        .await
        .expect_err("a boolean is not a read request");
    assert!(error.to_string().contains("a boolean"), "{error}");
    let error = read
        .execute(ToolCtx::new("c1"), json!({"server": "alpha"}))
        .await
        .expect_err("a request without a uri is refused");
    assert!(error.to_string().contains("uri"), "{error}");
}
