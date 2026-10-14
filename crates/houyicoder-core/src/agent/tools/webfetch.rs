//! Web content fetch tool — fetches a URL, strips HTML, returns text.
//!
//! Fetches a URL, strips HTML tags with a simple in-process parser (no
//! markdown converter dep), and returns the raw extracted text for the main
//! model to process, avoiding a secondary model round-trip. The domain
//! blocklist preflight, LRU cache, and same-host-only redirect restriction
//! are deferred.
//!
//! Read-only and concurrency-safe: the tool performs GET requests with no
//! side effects on the workspace. Failures become tool-result content (an
//! error JSON object) so the model sees the failure and can react.

use houyicoder_async::PFut;
use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolError};
use reqwest::Client;
use reqwest::redirect::Policy;
use std::time::Duration;

/// Maximum URL length accepted by the tool. Guards against oversized URLs
/// that may be crafted for data exfiltration.
const MAX_URL_LENGTH: usize = 2_000;

/// Maximum characters of extracted text returned to the model. Bounds
/// context consumption so a single fetch cannot overflow the window. Uses
/// 50k as a tighter default since no secondary model summarization happens
/// before the result enters the context.
const MAX_CONTENT_CHARS: usize = 50_000;

/// Request timeout in seconds. Prevents hanging on unresponsive servers.
const FETCH_TIMEOUT_SECS: u64 = 60;

/// Maximum redirect hops followed. Guards against redirect loops. Caps at
/// 10. This v1 follows all redirects (including cross-host) rather than
/// restricting to same-host.
const MAX_REDIRECTS: usize = 10;

/// A web content fetch tool. Fetches a URL, converts HTML to plain text,
/// truncates, and returns the content for the model to process.
///
/// The tool stores a reusable HTTP client (connection pool) behind a cheap
/// Arc clone. Construction is infallible unless the TLS backend cannot be
/// initialized (a system-level failure).
pub struct WebFetchTool {
    client: Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = Client::builder()
            .redirect(Policy::limited(MAX_REDIRECTS))
            .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
            .build()
            .expect("HTTP client construction failed (TLS backend error)");
        Self { client }
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
                    let body = resp.text().await.map_err(|e| {
                        ToolError::Failed(format!("webfetch: body read error: {e}"))
                    })?;

                    let text = if content_type.contains("text/html") {
                        strip_html(&body)
                    } else {
                        body
                    };
                    let truncated = truncate_content(&text, MAX_CONTENT_CHARS);

                    Ok(json!({
                        "content": truncated,
                        "url": url,
                        "prompt": prompt,
                        "status": status.as_u16(),
                        "content_type": content_type,
                    }))
                }
                Err(e) => Ok(json!({"error": format!("webfetch: fetch failed: {e}")})),
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
    true
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
