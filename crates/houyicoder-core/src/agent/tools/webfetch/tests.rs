//! Tests for this module.

use super::*;
use pollster::block_on;

#[test]
fn test_schema_has_url_prompt() {
    let tool = WebFetchTool::new();
    let schema = tool.input_schema();
    assert_eq!(schema["type"], "object");
    let props = &schema["properties"];
    assert!(props.get("url").is_some());
    assert!(props.get("prompt").is_some());
    let required = schema["required"].as_array().expect("required array");
    assert!(required.iter().any(|v| v == "url"));
    assert!(required.iter().any(|v| v == "prompt"));
}

#[test]
fn test_invalid_url_returns_error() {
    let tool = WebFetchTool::new();
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        json!({"url": "not-a-url", "prompt": "test"}),
    ))
    .expect("execute should succeed with error payload");
    assert!(out.get("error").is_some(), "expected error field: {out}");
    let err = out["error"].as_str().expect("error string");
    assert!(err.contains("Invalid URL"));
}

#[test]
fn test_missing_fields_error() {
    let tool = WebFetchTool::new();
    let result =
        block_on(tool.execute(ToolCtx::new("test"), json!({"url": "https://example.com"})));
    assert!(result.is_err());
}

#[test]
fn test_validate_url_accepts_valid() {
    assert!(validate_url("https://example.com/page"));
    assert!(validate_url("http://example.com"));
    assert!(validate_url("https://docs.example.com/path?q=1"));
}

#[test]
fn test_validate_url_rejects_bad() {
    assert!(!validate_url("not-a-url"));
    assert!(!validate_url("ftp://example.com"));
    assert!(!validate_url("https://localhost"));
    assert!(!validate_url("https://user:pass@example.com"));
    assert!(!validate_url(&"x".repeat(MAX_URL_LENGTH + 1)));
}

#[test]
fn test_upgrade_https() {
    assert_eq!(
        upgrade_to_https("http://example.com"),
        "https://example.com"
    );
    assert_eq!(
        upgrade_to_https("https://example.com/path"),
        "https://example.com/path"
    );
}

#[test]
fn test_strip_html_basic() {
    let html = "<html><body><p>Hello world</p></body></html>";
    let text = strip_html(html);
    assert!(text.contains("Hello world"));
    assert!(!text.contains('<'));
}

#[test]
fn test_strip_removes_script() {
    let html = "<p>before</p><script>alert('x')</script><p>after</p>";
    let text = strip_html(html);
    assert!(text.contains("before"));
    assert!(text.contains("after"));
    assert!(!text.contains("alert"));
}

#[test]
fn test_strip_removes_style() {
    let html = "<style>body { color: red; }</style><p>visible</p>";
    let text = strip_html(html);
    assert!(text.contains("visible"));
    assert!(!text.contains("color"));
}

#[test]
fn test_decode_entities() {
    let s = "&amp;&lt;&gt;&quot;&#39;&nbsp;";
    assert_eq!(decode_entities(s), "&<>\"' ");
}

#[test]
fn test_collapse_whitespace() {
    let s = "  hello   world  \n\n\n  next   line  ";
    let out = collapse_whitespace(s);
    assert_eq!(out, "hello world\n\nnext line");
}

#[test]
fn test_truncate_short_passes() {
    let s = "short content";
    assert_eq!(truncate_content(s, 100), s);
}

#[test]
fn test_truncate_long_cuts() {
    let s = "a".repeat(MAX_CONTENT_CHARS + 100);
    let out = truncate_content(&s, MAX_CONTENT_CHARS);
    assert!(out.contains("truncated"));
    assert!(out.chars().count() < s.chars().count());
}
