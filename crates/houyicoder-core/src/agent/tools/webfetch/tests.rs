//! Tests for this module.

use super::*;
use houyicoder_protocol::tool::tool_result_failed;
use pollster::block_on;
use reqwest::Client;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::thread::sleep;
use std::thread::spawn;
use std::time::Duration;
use tokio::runtime::Builder;

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

/// The remembered-failure map is bounded like the page cache, and the entry
/// it drops is the oldest one: a session that probes many distinct dead URLs
/// cannot grow it without limit, and the newest failure still answers.
#[test]
fn test_failure_cache_evicts_oldest() {
    let mut cache = FetchCache::default();
    let base = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);
    for i in 0..FAILURE_MAX_ENTRIES {
        cache.failures.insert(
            format!("https://example.com/dead/{i}"),
            FailedUrl {
                kind: "connect",
                message: "refused".to_string(),
                failed: base + Duration::from_millis(i as u64),
            },
        );
    }
    cache.store_failure(
        "https://example.com/dead/new",
        "connect",
        "refused".to_string(),
    );
    assert_eq!(
        cache.failures.len(),
        FAILURE_MAX_ENTRIES,
        "the failure cache is bounded"
    );
    assert!(
        !cache.failures.contains_key("https://example.com/dead/0"),
        "the oldest entry is the one dropped"
    );
    assert!(
        cache.failures.contains_key("https://example.com/dead/new"),
        "the newest failure still answers"
    );
    // Re-storing a URL already held replaces it in place: no unrelated entry
    // is evicted when the map does not grow. The refreshed key is deliberately
    // not the oldest, so an implementation that evicts before inserting shows
    // up as the oldest survivor going missing.
    cache.store_failure(
        "https://example.com/dead/new",
        "connect",
        "refused again".to_string(),
    );
    assert_eq!(cache.failures.len(), FAILURE_MAX_ENTRIES);
    assert!(
        cache.failures.contains_key("https://example.com/dead/1"),
        "a refresh must not evict an unrelated entry"
    );
}

/// Private and loopback targets are rejected before any request is sent, and
/// the same rule runs on a redirect hop.
#[test]
fn test_private_hosts_rejected() {
    for url in [
        "https://127.0.0.1/x",
        "https://10.0.0.5/x",
        "https://172.16.3.4/x",
        "https://192.168.1.1/x",
        "https://169.254.1.1/x",
        "https://[::1]/x",
        "https://printer.local/x",
    ] {
        assert!(!validate_url(url), "private target accepted: {url}");
    }
    assert!(validate_url("https://example.com/x"));
}

/// A short page that says it is missing is flagged and not cached, while a
/// long document that mentions the phrase stays ordinary content.
#[test]
fn test_soft_404_signal() {
    assert!(looks_like_missing_page(
        "# Page Not Found\n\nThis page does not exist."
    ));
    let long = format!("{} page not found", "x".repeat(2_000));
    assert!(!looks_like_missing_page(&long));
}

/// A body that would pass the cap stops at the chunk that crosses it, and the
/// payload names the reason. A body at the cap is still accepted.
#[test]
fn test_body_cap_stops() {
    let mut body = Vec::new();
    append_chunk(&mut body, b"hello").expect("under the cap");
    assert_eq!(body, b"hello");
    let too_big = vec![0u8; MAX_BODY_BYTES + 1];
    let err = append_chunk(&mut body, &too_big).expect_err("over the cap");
    assert_eq!(err["error_kind"], "response_too_large");
    assert_eq!(body, b"hello", "the rejected chunk is not appended");
}

/// Invalid bytes are replaced rather than failing the read.
#[test]
fn test_body_decode_lossy() {
    assert_eq!(decode_body(b"plain"), "plain");
    assert!(decode_body(&[0xff, b'a']).ends_with('a'));
}

/// A missing-page body is flagged and left out of the reuse cache, so a later
/// request is not answered with the same dead end.
#[test]
fn test_finish_page_flags_missing() {
    let cache = Mutex::new(FetchCache::default());
    let out = finish_page(
        &cache,
        "https://example.com/gone",
        "p",
        "# Page Not Found\n\nThis page does not exist.".to_string(),
        "text/markdown".to_string(),
    );
    assert_eq!(out["soft_404"], true);
    assert!(
        cache
            .lock()
            .expect("cache")
            .page("https://example.com/gone")
            .is_none(),
        "a missing page is not cached"
    );
}

/// A body that stopped arriving reports the read stage rather than a bare
/// message, so the model can tell it from a rejected request.
#[test]
fn test_read_error_payload() {
    let payload = read_error("connection reset");
    assert_eq!(payload["error_kind"], "read");
    assert!(
        payload["error"]
            .as_str()
            .expect("error")
            .contains("connection reset")
    );
}

/// The host check reads literal addresses in both families and the
/// non-canonical IPv4 spellings a resolver still accepts.
#[test]
fn test_host_check_families() {
    assert!(is_private_host("[::1]"));
    assert!(is_private_host("127.1"));
    assert!(is_private_host("0177.0.0.1"));
    assert!(!is_private_host("[2001:db8::1]"));
    assert!(!is_private_host("example.com"));
}

/// A hop is followed only while the count is under the cap and the target
/// passes the literal rule; a malformed base or target is refused with a
/// payload the model can read.
#[test]
fn test_redirect_hop_decides() {
    assert!(matches!(
        redirect_hop("https://example.com/a", None, 0),
        Hop::Done
    ));
    match redirect_hop("https://example.com/a", Some("/b"), 0) {
        Hop::Follow(next) => assert_eq!(next, "https://example.com/b"),
        _ => panic!("a relative hop is followed"),
    }
    assert!(matches!(
        redirect_hop("https://example.com/a", Some("/b"), MAX_REDIRECTS),
        Hop::Refuse(_)
    ));
    assert!(matches!(
        redirect_hop("https://example.com/a", Some("https://127.0.0.1/b"), 1),
        Hop::Refuse(_)
    ));
    assert!(matches!(
        redirect_hop("not a url", Some("/b"), 1),
        Hop::Refuse(_)
    ));
    assert!(matches!(
        redirect_hop("https://example.com/a", Some("http://[bad"), 1),
        Hop::Refuse(_)
    ));
}

/// The follower screens every hop: a hop to a target the URL rule refuses is
/// refused before it is sent. The test client follows nothing either, so the
/// only way to reach the refused target is the follower's own code.
#[test]
fn test_redirect_hop_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _read = stream.read(&mut buf);
        stream
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .expect("write");
        sleep(Duration::from_millis(50));
    });
    // The system proxy is off: this exercises the redirect chain against a
    // local server, and a proxy would answer for a name it cannot resolve.
    let client = Client::builder()
        .resolve("example.test", addr)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("client");
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let url = format!("http://example.test:{}/start", addr.port());
        let refused = fetch_following(&client, &url)
            .await
            .expect_err("the hop is refused");
        match refused {
            FetchError::Refused(payload) => assert_eq!(payload["error_kind"], "redirect"),
            FetchError::Transport(error) => {
                panic!("the hop was sent instead of refused: {error}")
            }
        }
    });
    server.join().expect("server");
}

/// A relative redirect is followed to the answer.
#[test]
fn test_follows_relative_redirect() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = spawn(move || {
        for hop in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _read = stream.read(&mut buf);
            let reply: &[u8] = if hop == 0 {
                b"HTTP/1.1 302 Found\r\nLocation: /second\r\nContent-Length: 0\r\n\r\n"
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
            };
            stream.write_all(reply).expect("write");
        }
    });
    // The system proxy is off: this exercises the redirect chain against a
    // local server, and a proxy would answer for a name it cannot resolve.
    let client = Client::builder()
        .resolve("example.test", addr)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .expect("client");
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let url = format!("http://example.test:{}/start", addr.port());
        let response = fetch_following(&client, &url).await.expect("response");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(read_body(response).await.expect("body"), "ok");
    });
    server.join().expect("server");
}

/// The body reader takes a real response in chunks and decodes it as text.
#[test]
fn test_read_body_from_response() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _read = stream.read(&mut buf);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/plain\r\n\r\nhello",
            )
            .expect("write");
    });
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let resp = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client")
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("request");
        let body = read_body(resp).await.expect("body");
        assert_eq!(body, "hello");
    });
    server.join().expect("server");
}

/// A name whose answer is loopback is refused by the resolver the connection
/// uses, and the host is looked up exactly once, so no second answer can point
/// somewhere the screen never saw.
#[test]
fn test_private_answer_refused() {
    let resolver = Arc::new(PublicResolver::with_fixed(vec![
        "127.0.0.1:80".parse().expect("address parses"),
    ]));
    let tool = WebFetchTool::with_resolver(Arc::clone(&resolver));
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let out = runtime
        .block_on(tool.execute(
            ToolCtx::new("test"),
            json!({"url": "http://example.test/x", "prompt": "read"}),
        ))
        .expect("tool result");
    assert_eq!(
        out["error_kind"], "private_target",
        "the refusal keeps its own kind: {out}"
    );
    assert_eq!(
        resolver.lookup_count(),
        1,
        "one lookup per hop, which is the lookup the connection uses"
    );
}

/// A cached failure tells the model to change source instead of retrying the
/// same dead link.
#[test]
fn test_cached_failure_hint() {
    let out = cached_failure_payload("connect_timeout", "timed out");
    assert_eq!(out["cached_failure"], true);
    assert_eq!(out["error_kind"], "connect_timeout");
    assert!(
        out["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("different source"),
        "the hint names the action: {out}"
    );
}

/// Every non-public range the resolver can answer with is refused, including
/// the IPv6 unique-local, link-local and IPv4-mapped forms.
#[test]
fn test_private_ip_ranges() {
    for ip in [
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.1.1",
        "0.0.0.0",
        "255.255.255.255",
        "224.0.0.1",
        "192.0.2.1",
        "100.64.0.1",
        "::1",
        "fc00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
    ] {
        let ip: IpAddr = ip.parse().expect("ip parses");
        assert!(is_private_ip(ip), "{ip} is not public");
    }
    assert!(!is_private_ip("8.8.8.8".parse().expect("ip")));
}

/// A literal private address and a loopback name are refused before any
/// lookup, so a malformed or literal URL never reaches the resolver.
#[test]
fn test_literal_private_refused() {
    for url in [
        "http://127.0.0.1/x",
        "http://localhost/x",
        "http://10.0.0.1/x",
        "http://[::1]/x",
    ] {
        assert!(
            !validate_url(url),
            "a private literal is refused by the URL rule: {url}"
        );
    }
    assert!(
        validate_url("https://example.test/x"),
        "a public literal passes the URL rule"
    );
}

/// A hex literal is a numeric host; a word made of hex letters is a domain.
#[test]
fn test_numeric_host_guard() {
    assert!(is_private_host("0x7f.0.0.1"));
    assert!(is_private_host("0x7f000001"));
    assert!(!is_private_host("dec.af"));
    assert!(!is_private_host("face.be"));
}
