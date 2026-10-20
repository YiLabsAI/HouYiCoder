//! Tests for this module.

use super::*;
use houyicoder_protocol::tool::tool_result_failed;
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

/// A page fetched once is reused for the same URL instead of being fetched
/// again, and the cache stays bounded.
#[test]
fn test_cache_reuses_page() {
    let mut cache = FetchCache::default();
    cache.store_page(
        "https://example.com/a",
        "body".to_string(),
        "text/markdown".to_string(),
    );
    let reused = cache.page("https://example.com/a");
    assert_eq!(
        reused,
        Some(("body".to_string(), "text/markdown".to_string()))
    );
    assert_eq!(cache.page("https://example.com/b"), None);
    for i in 0..=CACHE_MAX_ENTRIES {
        cache.store_page(
            &format!("https://example.com/{i}"),
            "x".to_string(),
            "text/plain".to_string(),
        );
    }
    assert_eq!(
        cache.pages.len(),
        CACHE_MAX_ENTRIES,
        "the page cache is bounded"
    );
}

/// A URL that just failed to connect is answered from that failure, so a
/// blind retry in the same turn does not pay the timeout again.
#[test]
fn test_cache_answers_failure() {
    let mut cache = FetchCache::default();
    cache.store_failure(
        "https://example.com/dead",
        "tls",
        "certificate verify failed".to_string(),
    );
    assert_eq!(
        cache.failure("https://example.com/dead"),
        Some(("tls", "certificate verify failed".to_string()))
    );
    assert_eq!(cache.failure("https://example.com/live"), None);
}

/// An error status is a failure the transcript and the trajectory can count.
/// The previous shape carried the error page as content with a status field,
/// which the shared failure rule read as a success.
#[test]
fn test_http_status_is_failure() {
    let failure = json!({
        "error": "webfetch: http_status: HTTP 404 Not Found",
        "error_kind": "http_status",
        "status": 404,
    });
    assert!(tool_result_failed(&failure, "WebFetch", &json!({})));
    let previous_shape = json!({"content": "# Page Not Found", "status": 404});
    assert!(
        !tool_result_failed(&previous_shape, "WebFetch", &json!({})),
        "the old shape was the bug: an error page counted as content"
    );
}

/// A cached page past its reuse window is dropped rather than served stale.
#[test]
fn test_cache_drops_expired_page() {
    let mut cache = FetchCache::default();
    cache.store_page(
        "https://example.com/old",
        "body".to_string(),
        "text/plain".to_string(),
    );
    let later = Instant::now() + PAGE_TTL;
    assert_eq!(cache.page_at("https://example.com/old", later), None);
    assert!(
        !cache.pages.contains_key("https://example.com/old"),
        "the expired page stays in the cache"
    );
}

/// A remembered failure past its short window is dropped, so a host that
/// recovers is reachable again.
#[test]
fn test_cache_drops_expired_failure() {
    let mut cache = FetchCache::default();
    cache.store_failure("https://example.com/dead", "connect", "refused".to_string());
    let later = Instant::now() + FAILURE_TTL;
    assert_eq!(cache.failure_at("https://example.com/dead", later), None);
    assert!(
        !cache.failures.contains_key("https://example.com/dead"),
        "the expired failure stays in the cache"
    );
}

/// A cached page is answered without a request, and the result says it came
/// from the cache.
#[test]
fn test_execute_serves_cached_page() {
    let tool = WebFetchTool::new();
    tool.cache.lock().expect("cache").store_page(
        "https://example.com/page",
        "cached body".to_string(),
        "text/plain".to_string(),
    );
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        json!({"url": "https://example.com/page", "prompt": "what"}),
    ))
    .expect("execute should return the cached payload");
    assert_eq!(out["content"], "cached body");
    assert_eq!(out["cached"], true);
    assert!(out.get("error").is_none(), "a cache hit is not a failure");
}

/// A URL whose connection just failed is answered from that failure instead
/// of paying the timeout again, and the payload still names the stage.
#[test]
fn test_execute_serves_cached_failure() {
    let tool = WebFetchTool::new();
    tool.cache.lock().expect("cache").store_failure(
        "https://example.com/dead",
        "tls",
        "certificate verify failed".to_string(),
    );
    let out = block_on(tool.execute(
        ToolCtx::new("test"),
        json!({"url": "https://example.com/dead", "prompt": "what"}),
    ))
    .expect("execute should return the remembered failure");
    assert_eq!(out["error_kind"], "tls");
    assert_eq!(out["cached_failure"], true);
    assert!(
        out["error"].as_str().expect("error").contains("tls"),
        "the payload names the stage: {out}"
    );
}

/// Every transport stage has its own label, so the model can tell a host it
/// cannot reach from a request that failed after the response started.
#[test]
fn test_classify_failure_stages() {
    assert_eq!(classify_failure(true, true, "timed out"), "connect_timeout");
    assert_eq!(classify_failure(true, false, "timed out"), "read_timeout");
    assert_eq!(
        classify_failure(false, true, "certificate verify failed"),
        "tls"
    );
    assert_eq!(
        classify_failure(false, true, "connection refused"),
        "connect"
    );
    assert_eq!(classify_failure(false, false, "bad request"), "request");
}

/// The page payload carries the cache flag only when it was reused.
#[test]
fn test_page_payload_marks_reuse() {
    let fresh = page_payload("u", "p", "c".to_string(), "text/plain".to_string(), false);
    assert!(fresh.get("cached").is_none(), "a fresh page is not cached");
    let reused = page_payload("u", "p", "c".to_string(), "text/plain".to_string(), true);
    assert_eq!(reused["cached"], true);
}

/// An error status produces a failure payload, never content with a status
/// field: the error page must not read as the page's text.
#[test]
fn test_http_failure_payload() {
    let payload = http_failure_payload("https://example.com/x", 404, "Not Found");
    assert_eq!(payload["error_kind"], "http_status");
    assert_eq!(payload["status"], 404);
    assert_eq!(payload["url"], "https://example.com/x");
    assert!(
        payload.get("content").is_none(),
        "an error page is not content: {payload}"
    );
    assert!(tool_result_failed(&payload, "WebFetch", &json!({})));
}

/// A successful body becomes the tool result and is kept for reuse: HTML is
/// reduced to text, plain text passes through, and both land in the cache.
#[test]
fn test_finish_page_strips_html() {
    let cache = Mutex::new(FetchCache::default());
    let html = finish_page(
        &cache,
        "https://example.com/h",
        "p",
        "<p>Hello</p>".to_string(),
        "text/html".to_string(),
    );
    assert_eq!(html["content"], "Hello");
    assert_eq!(html["status"], 200);
    assert!(
        cache
            .lock()
            .expect("cache")
            .page("https://example.com/h")
            .is_some(),
        "a successful page is kept for reuse"
    );
    let plain = finish_page(
        &cache,
        "https://example.com/p",
        "p",
        "plain text".to_string(),
        "text/plain".to_string(),
    );
    assert_eq!(plain["content"], "plain text");
}

/// The remembered-failure map is bounded like the page cache, so a session
/// that probes many distinct dead URLs cannot grow it without limit.
#[test]
fn test_failure_cache_bounded() {
    let mut cache = FetchCache::default();
    for i in 0..=FAILURE_MAX_ENTRIES {
        cache.store_failure(
            &format!("https://example.com/dead/{i}"),
            "connect",
            "refused".to_string(),
        );
    }
    assert_eq!(
        cache.failures.len(),
        FAILURE_MAX_ENTRIES,
        "the failure cache is bounded"
    );
}
