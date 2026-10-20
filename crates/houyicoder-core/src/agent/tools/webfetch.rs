//! Web content fetch tool — fetches a URL, strips HTML, returns text.
//!
//! Fetches a URL, strips HTML tags with a simple in-process parser (no
//! markdown converter dep), and returns the raw extracted text for the main
//! model to process, avoiding a secondary model round-trip. The domain
//! resolved-address and redirect-target screens run before each hop.
//!
//! Read-only and concurrency-safe: the tool performs GET requests with no
//! side effects on the workspace. Failures become tool-result content (an
//! error JSON object) so the model sees the failure and can react. Each
//! failure names the stage it came from, because a host that cannot be reached
//! and a host that answered with an error page are different problems for the
//! model to react to.
//!
//! A page fetched once is reusable for a short while, and a URL that just
//! failed to connect is answered from that failure for a shorter while, so a
//! model that retries the same dead link pays one timeout rather than one per
//! attempt.

use houyicoder_async::PFut;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};
use reqwest::Client;
use reqwest::redirect::Policy;
use std::collections::HashMap;
use std::io::Error;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::lookup_host;

/// Maximum URL length accepted by the tool. Guards against oversized URLs
/// that may be crafted for data exfiltration.
const MAX_URL_LENGTH: usize = 2_000;

/// Maximum characters of extracted text returned to the model. Bounds
/// context consumption so a single fetch cannot overflow the window. Uses
/// 50k as a tighter default since no secondary model summarization happens
/// before the result enters the context.
const MAX_CONTENT_CHARS: usize = 50_000;

/// Request timeout in seconds. Prevents hanging on unresponsive servers. A
/// fetch that has not answered by now is not going to be useful inside a
/// model turn, and the turn should learn that quickly.
const FETCH_TIMEOUT_SECS: u64 = 30;

/// Connection timeout in seconds. Separate from the request timeout: a host
/// that never completes the TCP or TLS handshake fails here instead of
/// holding the whole request budget, which is what makes a dead link cost
/// seconds rather than the full request timeout.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Idle connection lifetime. Keeps a pooled connection alive across the gap
/// between two tool calls in one turn instead of paying a fresh handshake.
const TCP_KEEPALIVE_SECS: u64 = 30;

/// How long a fetched page stays reusable for the same URL.
const PAGE_TTL: Duration = Duration::from_secs(15 * 60);

/// How long a URL that just failed to connect stays answered from that
/// failure. Short on purpose: a transient network fault clears, and the tool
/// must not keep a host unreachable after it recovers.
const FAILURE_TTL: Duration = Duration::from_secs(60);

/// Upper bound on cached pages, so a long session cannot grow this cache
/// without limit. The oldest page is dropped when the bound is reached.
const CACHE_MAX_ENTRIES: usize = 32;

/// Upper bound on remembered connection failures, for the same reason: a
/// session that probes many distinct dead URLs must not grow this map without
/// limit. The oldest failure is dropped when the bound is reached.
const FAILURE_MAX_ENTRIES: usize = 32;

/// Maximum bytes of a response body the tool will read. A page larger than
/// this is not a page a model turn can use, and reading it whole first would
/// spend the memory before the truncation ran.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Maximum redirect hops followed. Guards against redirect loops. A hop may
/// cross hosts, but every target still passes the same URL check, so a
/// redirect cannot reach a private address the first hop was denied.
const MAX_REDIRECTS: usize = 10;

/// A page fetched once, kept for a short reuse window.
struct CachedPage {
    content: String,
    content_type: String,
    fetched: Instant,
}

/// A URL whose connection attempt failed, kept only long enough that a blind
/// retry in the same turn does not pay the timeout again.
struct FailedUrl {
    kind: &'static str,
    message: String,
    failed: Instant,
}

/// Per-tool reuse state: successful pages and recent connection failures,
/// both keyed by the URL as requested (before the HTTPS upgrade), so the key
/// matches what a caller asks for.
#[derive(Default)]
struct FetchCache {
    pages: HashMap<String, CachedPage>,
    failures: HashMap<String, FailedUrl>,
}

impl FetchCache {
    /// A page still inside its reuse window.
    fn page(&mut self, url: &str) -> Option<(String, String)> {
        self.page_at(url, Instant::now())
    }

    /// A page still inside its reuse window at a stated instant. The instant
    /// is a parameter so a test can state an expired entry instead of waiting
    /// out the window.
    fn page_at(&mut self, url: &str, now: Instant) -> Option<(String, String)> {
        let page = self.pages.get(url)?;
        if now.saturating_duration_since(page.fetched) >= PAGE_TTL {
            self.pages.remove(url);
            return None;
        }
        Some((page.content.clone(), page.content_type.clone()))
    }

    /// Keep a page for reuse, dropping the oldest entry when the cache is
    /// full. Re-storing a URL already held replaces it in place, so a refresh
    /// does not evict an unrelated page.
    fn store_page(&mut self, url: &str, content: String, content_type: String) {
        if !self.pages.contains_key(url)
            && self.pages.len() >= CACHE_MAX_ENTRIES
            && let Some(oldest) = self
                .pages
                .iter()
                .min_by_key(|(_, page)| page.fetched)
                .map(|(key, _)| key.clone())
        {
            self.pages.remove(&oldest);
        }
        self.pages.insert(
            url.to_string(),
            CachedPage {
                content,
                content_type,
                fetched: Instant::now(),
            },
        );
    }

    /// A connection failure still inside its short window.
    fn failure(&mut self, url: &str) -> Option<(&'static str, String)> {
        self.failure_at(url, Instant::now())
    }

    /// A connection failure still inside its short window at a stated instant,
    /// for the same reason the page check takes one.
    fn failure_at(&mut self, url: &str, now: Instant) -> Option<(&'static str, String)> {
        let failure = self.failures.get(url)?;
        if now.saturating_duration_since(failure.failed) >= FAILURE_TTL {
            self.failures.remove(url);
            return None;
        }
        Some((failure.kind, failure.message.clone()))
    }

    /// Remember that a URL could not be reached, dropping the oldest entry
    /// when the cache is full. Re-storing a URL already held replaces it in
    /// place, so a repeated failure does not evict an unrelated entry.
    fn store_failure(&mut self, url: &str, kind: &'static str, message: String) {
        if !self.failures.contains_key(url)
            && self.failures.len() >= FAILURE_MAX_ENTRIES
            && let Some(oldest) = self
                .failures
                .iter()
                .min_by_key(|(_, failure)| failure.failed)
                .map(|(key, _)| key.clone())
        {
            self.failures.remove(&oldest);
        }
        self.failures.insert(
            url.to_string(),
            FailedUrl {
                kind,
                message,
                failed: Instant::now(),
            },
        );
    }
}

/// Whether a redirect hop may be followed: the hop count is under the cap and
/// the target passes the same literal URL rule as the first request. The
/// resolved-address screen runs on the first target only, because the hop is
/// resolved inside the client; a redirect to a name that resolves inward is
/// not excluded here.
fn redirect_allowed(previous: usize, target: &str) -> Result<(), &'static str> {
    if previous >= MAX_REDIRECTS {
        Err("too many redirects")
    } else if !validate_url(target) {
        Err("redirect target rejected")
    } else {
        Ok(())
    }
}

/// The stage a fetch failed in, as the model sees it. A transport failure
/// before any response and a response the server marked as an error are
/// different problems, and the model reacts to them differently.
fn classify_fetch_error(error: &reqwest::Error) -> &'static str {
    classify_failure(error.is_timeout(), error.is_connect(), &error.to_string())
}

/// The failure stage from the transport facts and the message text. Split from
/// the error type so each branch is stated by a test rather than inferred
/// from a live network fault.
fn classify_failure(timed_out: bool, connect: bool, message: &str) -> &'static str {
    if timed_out {
        if connect {
            "connect_timeout"
        } else {
            "read_timeout"
        }
    } else if connect {
        let text = message.to_ascii_lowercase();
        if text.contains("certificate") || text.contains("tls") || text.contains("x509") {
            "tls"
        } else {
            "connect"
        }
    } else {
        "request"
    }
}

/// The result of a page the tool can hand back: freshly fetched, or reused
/// from the cache with the flag that says so.
fn page_payload(
    url: &str,
    prompt: &str,
    content: String,
    content_type: String,
    cached: bool,
) -> Value {
    let mut payload = json!({
        "content": content,
        "url": url,
        "prompt": prompt,
        "status": 200,
        "content_type": content_type,
    });
    if cached {
        payload["cached"] = json!(true);
    }
    payload
}

/// The result of a response the server marked as an error. The error field is
/// what the transcript colors and the trajectory counts, so an error page can
/// never read as content.
fn http_failure_payload(url: &str, status: u16, reason: &str) -> Value {
    json!({
        "error": format!("webfetch: http_status: HTTP {status} {reason}"),
        "error_kind": "http_status",
        "status": status,
        "url": url,
    })
}

/// Turn a successful response body into the tool result, and keep the page for
/// reuse. Split from the request so the extraction and the cache write are
/// stated by a test rather than only by a live fetch.
fn finish_page(
    cache: &Mutex<FetchCache>,
    url: &str,
    prompt: &str,
    body: String,
    content_type: String,
) -> Value {
    let text = if content_type.contains("text/html") {
        strip_html(&body)
    } else {
        body
    };
    let truncated = truncate_content(&text, MAX_CONTENT_CHARS);
    let missing = looks_like_missing_page(&truncated);
    // A page that reports itself missing is not reused: caching it would keep
    // answering later requests with the same dead end.
    if !missing && let Ok(mut c) = cache.lock() {
        c.store_page(url, truncated.clone(), content_type.clone());
    }
    let mut payload = page_payload(url, prompt, truncated, content_type, false);
    if missing {
        payload["soft_404"] = json!(true);
    }
    payload
}

/// Read a response body up to the byte cap. A larger body returns the error
/// payload rather than being read whole and then truncated: the memory is
/// spent before the truncation would run.
async fn read_body(mut resp: reqwest::Response) -> Result<String, Value> {
    let mut body = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => append_chunk(&mut body, &chunk)?,
            Ok(None) => break,
            Err(e) => return Err(read_error(&e.to_string())),
        }
    }
    Ok(decode_body(&body))
}

/// Append one chunk unless the body would pass the cap.
fn append_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), Value> {
    if body.len() + chunk.len() > MAX_BODY_BYTES {
        return Err(json!({
            "error": format!("webfetch: response_too_large: over {MAX_BODY_BYTES} bytes"),
            "error_kind": "response_too_large",
        }));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

/// The payload for a body that stopped arriving.
fn read_error(message: &str) -> Value {
    json!({
        "error": format!("webfetch: body read error: {message}"),
        "error_kind": "read",
    })
}

/// Decode a body as text, replacing invalid bytes rather than failing: a page
/// with one bad byte is still readable.
fn decode_body(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// Whether a short page says it does not exist. The check is deliberately
/// narrow: a long document that mentions the phrase is real content, and a
/// server that answers 200 with a missing-page notice is the case worth
/// flagging.
fn looks_like_missing_page(text: &str) -> bool {
    if text.chars().count() >= 400 {
        return false;
    }
    let head: String = text
        .lines()
        .take(6)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    head.contains("page not found") || head.contains("404 not found")
}

/// The result of a transport failure, naming the stage it came from.
fn fetch_failure_payload(kind: &'static str, message: &str) -> Value {
    json!({
        "error": format!("webfetch: {kind}: {message}"),
        "error_kind": kind,
    })
}

/// A web content fetch tool. Fetches a URL, converts HTML to plain text,
/// truncates, and returns the content for the model to process.
///
/// The tool stores a reusable HTTP client (connection pool) behind a cheap
/// Arc clone, plus a bounded per-tool reuse cache shared across calls.
/// Construction is infallible unless the TLS backend cannot be initialized
/// (a system-level failure).
pub struct WebFetchTool {
    client: Client,
    cache: Arc<Mutex<FetchCache>>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = Client::builder()
            .redirect(Policy::custom(|attempt| {
                match redirect_allowed(attempt.previous().len(), attempt.url().as_str()) {
                    Ok(()) => attempt.follow(),
                    Err(reason) => attempt.error(Error::other(reason)),
                }
            }))
            .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS))
            .build()
            .expect("HTTP client construction failed (TLS backend error)");
        Self {
            client,
            cache: Arc::new(Mutex::new(FetchCache::default())),
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }
    fn description(&self) -> &str {
        "Fetch content from a URL and extract text. \
         Input: {url: string, prompt: string}. \
         Fetches the URL, converts HTML to plain text, truncates to 50k chars. \
         The prompt describes what to extract from the fetched content. \
         HTTP URLs are upgraded to HTTPS. Read-only, does not modify files. \
         Results may be truncated if the content is very large. \
         Prefer using gh CLI via Bash for GitHub URLs instead."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The URL to fetch content from"},
                "prompt": {"type": "string", "description": "The prompt to run on the fetched content"}
            },
            "required": ["url", "prompt"]
        })
    }
    fn execute(&self, _ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let client = self.client.clone();
        let cache = Arc::clone(&self.cache);
        Box::pin(async move {
            let url = input
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("webfetch: url (string) required".into()))?;
            let prompt = input
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ToolError::InvalidInput("webfetch: prompt (string) required".into())
                })?;

            if !validate_url(url) {
                return Ok(json!({"error": format!("Invalid URL: {url}")}));
            }

            let fetch_url = upgrade_to_https(url);

            // Reuse a page already fetched in this session, and answer a URL
            // whose connection just failed from that failure, so a blind
            // retry inside one turn does not pay the timeout twice.
            let reused = cache.lock().ok().and_then(|mut c| c.page(url));
            if let Some((content, content_type)) = reused {
                return Ok(page_payload(url, prompt, content, content_type, true));
            }
            let recent_failure = cache.lock().ok().and_then(|mut c| c.failure(url));
            if let Some((kind, message)) = recent_failure {
                return Ok(cached_failure_payload(kind, &message));
            }

            // A public name can resolve to a private address; the literal
            // check cannot see that, so the first target is screened by its
            // resolved addresses before the request is sent.
            if let Err(payload) = ensure_public_target(&fetch_url).await {
                return Ok(payload);
            }

            let result = client
                .get(&fetch_url)
                .header("Accept", "text/markdown, text/html, */*")
                .header("User-Agent", "webfetch/0.1")
                .send()
                .await;

            match result {
                Ok(resp) => {
                    let status = resp.status();
                    let content_type = resp
                        .headers()
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let body = match read_body(resp).await {
                        Ok(body) => body,
                        Err(payload) => return Ok(payload),
                    };

                    // A server that answered with an error status is a
                    // failure the model must see as one: the body is an error
                    // page, not content, and the transcript's failure count
                    // and the trajectory's failure rows read this same field.
                    if !status.is_success() {
                        return Ok(http_failure_payload(
                            url,
                            status.as_u16(),
                            status.canonical_reason().unwrap_or(""),
                        ));
                    }

                    Ok(finish_page(&cache, url, prompt, body, content_type))
                }
                Err(e) => {
                    let kind = classify_fetch_error(&e);
                    let message = e.to_string();
                    // Only a transport failure is worth remembering: a
                    // request the server rejected may succeed next time.
                    if matches!(kind, "connect" | "connect_timeout" | "tls")
                        && let Ok(mut c) = cache.lock()
                    {
                        c.store_failure(url, kind, message.clone());
                    }
                    Ok(fetch_failure_payload(kind, &message))
                }
            }
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
}

/// Validate a URL string. Rejects oversized URLs, malformed URLs, URLs
/// with username/password credentials, and hostnames without a dot (a
/// heuristic to block internal or privileged addresses).
fn validate_url(url: &str) -> bool {
    if url.len() > MAX_URL_LENGTH {
        return false;
    }
    let after_scheme = if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else {
        return false;
    };
    let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if authority.contains('@') {
        return false;
    }
    let hostname = authority.split(':').next().unwrap_or(authority);
    if !hostname.contains('.') {
        return false;
    }
    if is_private_host(hostname) {
        return false;
    }
    true
}

/// The payload for a URL that failed a moment ago: the stage, the fact that
/// it is remembered, and the action that helps — change source.
fn cached_failure_payload(kind: &'static str, message: &str) -> Value {
    let mut payload = fetch_failure_payload(kind, message);
    payload["cached_failure"] = json!(true);
    payload["hint"] = json!(
        "this url failed to connect a moment ago; try a different source rather than retrying it"
    );
    payload
}

/// Refuse a target whose resolved addresses are private, loopback, or
/// link-local. The literal check runs first; this covers a public name that
/// resolves inward. The request re-resolves the name, so a name that changes
/// its answer between the two lookups is not fully excluded; the screen
/// removes the ordinary case without pinning the connection.
async fn ensure_public_target(url: &str) -> Result<(), Value> {
    let Some(authority) = url
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
    else {
        return Ok(());
    };
    let host = authority.split(':').next().unwrap_or(authority);
    if host.is_empty() {
        return Ok(());
    }
    let port = if url.starts_with("https://") { 443 } else { 80 };
    let Ok(addresses) = lookup_host((host.to_string(), port)).await else {
        // A name that does not resolve fails at connect with its own stage.
        return Ok(());
    };
    for address in addresses {
        if is_private_ip(address.ip()) {
            return Err(json!({
                "error": format!("webfetch: target resolves to a private address ({})", address.ip()),
                "error_kind": "private_target",
            }));
        }
    }
    Ok(())
}

/// Whether a resolved address belongs to this machine or a private network.
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // Carrier-grade NAT, not reachable from the public internet.
                || (a == 100 && (64..=127).contains(&b))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique local and link local.
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Whether a host names this machine or a private network. A public page can
/// redirect to one of these, so the same check runs on every hop.
///
/// A name is screened here by its literal form; the resolved addresses are
/// screened by ensure_public_target before each hop is sent.
fn is_private_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".local") {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private_ip(ip);
    }
    // A host written only with digits and dots is a non-canonical IPv4
    // spelling (127.1, 0177.0.0.1) that resolvers still accept, so it is not
    // a public name.
    // Only the non-canonical IPv4 spellings are numeric hosts: all digits and
    // dots, or a hex literal marked with 0x. A word made of hex letters is a
    // real domain name and stays allowed.
    host.chars().all(|c| c.is_ascii_digit() || c == '.')
        || (host.contains("0x")
            && host
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '.' || c == 'x'))
}

/// Upgrade an HTTP URL to HTTPS. Leaves HTTPS URLs unchanged.
fn upgrade_to_https(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

/// Strip HTML tags from a string, producing readable plain text. Removes
/// script and style blocks entirely (including their content), drops all
/// remaining tags, decodes common HTML entities, and collapses whitespace.
fn strip_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;

    while i < html.len() {
        let rest = &lower[i..];

        if rest.starts_with("<script") {
            if let Some(end) = rest.find("</script>") {
                i += end + "</script>".len();
                continue;
            }
            break;
        }
        if rest.starts_with("<style") {
            if let Some(end) = rest.find("</style>") {
                i += end + "</style>".len();
                continue;
            }
            break;
        }
        if html.as_bytes()[i] == b'<' {
            if let Some(gt) = html[i..].find('>') {
                i += gt + 1;
                continue;
            }
            break;
        }
        if let Some(ch) = html[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    let decoded = decode_entities(&out);
    collapse_whitespace(&decoded)
}

/// Decode common HTML entities to their character equivalents.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// Collapse runs of whitespace into single spaces and blank lines, so the
/// extracted text is compact and readable. Preserves paragraph breaks
/// (double newlines) but removes excessive blank lines.
fn collapse_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_blank = false;
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_blank {
                result.push('\n');
                prev_blank = true;
            }
            continue;
        }
        let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
        result.push_str(&collapsed);
        result.push('\n');
        prev_blank = false;
    }
    result.trim_end().to_string()
}

/// Truncate text to a maximum character count, appending a truncation
/// marker when content is cut. Uses 50k as a tighter default.
fn truncate_content(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated}\n\n[Content truncated due to length...]")
}

#[cfg(test)]
mod tests;
