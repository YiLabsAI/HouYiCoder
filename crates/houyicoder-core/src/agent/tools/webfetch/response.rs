//! Reading a response body and shaping it into the tool result: the payload a
//! successful page returns, and the payload each failure stage returns.

use super::*;

/// The result of a page the tool can hand back: freshly fetched, or reused
/// from the cache with the flag that says so.
pub(super) fn page_payload(
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
pub(super) fn http_failure_payload(url: &str, status: u16, reason: &str) -> Value {
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
pub(super) fn finish_page(
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
pub(super) async fn read_body(mut resp: reqwest::Response) -> Result<String, Value> {
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
pub(super) fn append_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), Value> {
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
pub(super) fn cached_failure_payload(kind: &'static str, message: &str) -> Value {
    let mut payload = fetch_failure_payload(kind, message);
    payload["cached_failure"] = json!(true);
    payload["hint"] = json!(
        "this url failed to connect a moment ago; try a different source rather than retrying it"
    );
    payload
}

/// The payload for a body that stopped arriving.
pub(super) fn read_error(message: &str) -> Value {
    json!({
        "error": format!("webfetch: body read error: {message}"),
        "error_kind": "read",
    })
}

/// Decode a body as text, replacing invalid bytes rather than failing: a page
/// with one bad byte is still readable.
pub(super) fn decode_body(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// Whether a short page says it does not exist. The check is deliberately
/// narrow: a long document that mentions the phrase is real content, and a
/// server that answers 200 with a missing-page notice is the case worth
/// flagging.
pub(super) fn looks_like_missing_page(text: &str) -> bool {
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
pub(super) fn fetch_failure_payload(kind: &'static str, message: &str) -> Value {
    json!({
        "error": format!("webfetch: {kind}: {message}"),
        "error_kind": kind,
    })
}
