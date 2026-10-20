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

mod cache;
mod target;

#[cfg(test)]
pub(crate) use cache::CACHE_MAX_ENTRIES;
#[cfg(test)]
pub(crate) use cache::FAILURE_MAX_ENTRIES;
#[cfg(test)]
pub(crate) use cache::FAILURE_TTL;
#[cfg(test)]
pub(crate) use cache::FailedUrl;
pub(crate) use cache::FetchCache;
#[cfg(test)]
pub(crate) use cache::PAGE_TTL;
use reqwest::Client;
use reqwest::redirect::Policy;
#[cfg(test)]
pub(crate) use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(test)]
pub(crate) use std::time::Instant;
pub(crate) use target::FetchError;
#[cfg(test)]
pub(crate) use target::Hop;
#[cfg(test)]
pub(crate) use target::MAX_REDIRECTS;
#[cfg(test)]
pub(crate) use target::ensure_public_target;
pub(crate) use target::fetch_following;
#[cfg(test)]
pub(crate) use target::is_private_host;
#[cfg(test)]
pub(crate) use target::is_private_ip;
#[cfg(test)]
pub(crate) use target::redirect_hop;
pub(crate) use target::upgrade_to_https;
pub(crate) use target::validate_url;
use tokio::net::lookup_host;

/// Maximum URL length accepted by the tool. Guards against oversized URLs
/// that may be crafted for data exfiltration.
const MAX_URL_LENGTH: usize = 2_000;

/// Maximum bytes of a response body the tool will read. A page larger than
/// this is not a page a model turn can use, and reading it whole first would
/// spend the memory before the truncation ran.
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

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
            // Redirects are followed here so every hop passes the resolved
            // address screen, which the client's own policy cannot do.
            .redirect(Policy::none())
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

            let result = fetch_following(&client, &fetch_url).await;

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
                Err(FetchError::Refused(payload)) => Ok(payload),
                Err(FetchError::Transport(e)) => {
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
