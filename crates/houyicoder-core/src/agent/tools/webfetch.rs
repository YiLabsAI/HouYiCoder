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
mod resolver;
mod response;
mod route;
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
use resolver::PRIVATE_TARGET;
use resolver::PublicResolver;
#[cfg(test)]
use response::append_chunk;
use response::cached_failure_payload;
#[cfg(test)]
use response::decode_body;
use response::fetch_failure_payload;
use response::finish_page;
use response::http_failure_payload;
#[cfg(test)]
use response::looks_like_missing_page;
use response::page_payload;
use response::read_body;
#[cfg(test)]
use response::read_error;
pub(crate) use route::RouteClient;
use std::error::Error;
#[cfg(test)]
pub(crate) use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(test)]
pub(crate) use std::time::Instant;
pub(crate) use target::FetchError;
pub(crate) use target::fetch_following;
#[cfg(test)]
pub(crate) use target::is_private_host;
#[cfg(test)]
pub(crate) use target::is_private_ip;
pub(crate) use target::upgrade_to_https;
pub(crate) use target::validate_url;

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
    classify_failure(
        error.is_timeout(),
        error.is_connect(),
        &error_chain_text(error),
    )
}

/// The error and every cause beneath it, joined. The client's own Display
/// names only the request, so the reason a connection was refused is one link
/// down the chain.
fn error_chain_text(error: &reqwest::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(current) = source {
        parts.push(current.to_string());
        source = current.source();
    }
    parts.join(": ")
}

/// The failure stage from the transport facts and the message text. Split from
/// the error type so each branch is stated by a test rather than inferred
/// from a live network fault.
fn classify_failure(timed_out: bool, connect: bool, message: &str) -> &'static str {
    // The resolver refuses a name that answers with a private address. That
    // reaches the client as a connect failure, so the screen's own wording is
    // what tells this apart from an ordinary unreachable host.
    if message.contains(PRIVATE_TARGET) {
        return "private_target";
    }
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

/// A web content fetch tool. Fetches a URL, converts HTML to plain text,
/// truncates, and returns the content for the model to process.
///
/// The tool stores a reusable HTTP client (connection pool) behind a cheap
/// Arc clone, plus a bounded per-tool reuse cache shared across calls.
/// Construction is infallible unless the TLS backend cannot be initialized
/// (a system-level failure).
pub struct WebFetchTool {
    direct: Client,
    routed: Option<RouteClient>,
    resolver: Arc<PublicResolver>,
    cache: Arc<Mutex<FetchCache>>,
}

/// The builder every direct fetch client starts from. The system route is
/// turned off on this client so it serves as the clean fallback whenever the
/// mediated client is absent or unreachable.
fn direct_client(resolver: Arc<PublicResolver>) -> Client {
    Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS))
        .no_proxy()
        .dns_resolver(resolver)
        .build()
        .expect("HTTP client construction failed (TLS backend error)")
}

impl WebFetchTool {
    pub fn new() -> Self {
        let resolver = Arc::new(PublicResolver::new());
        let direct = direct_client(Arc::clone(&resolver));
        let routed = route::resolve_route_client(Arc::clone(&resolver));
        Self {
            direct,
            routed,
            resolver,
            cache: Arc::new(Mutex::new(FetchCache::default())),
        }
    }

    /// Build a tool whose names resolve through the given resolver.
    #[cfg(test)]
    pub(crate) fn with_resolver(resolver: Arc<PublicResolver>) -> Self {
        Self {
            direct: direct_client(Arc::clone(&resolver)),
            routed: None,
            resolver,
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
        let direct = self.direct.clone();
        let routed = self.routed.clone();
        let resolver = Arc::clone(&self.resolver);
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

            let result =
                fetch_following(&direct, routed.as_ref(), Some(&resolver), &fetch_url).await;

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
                    let message = error_chain_text(&e);
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
