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
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .timeout(std::time::Duration::from_secs(FETCH_TIMEOUT_SECS))
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
mod tests {
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
}
