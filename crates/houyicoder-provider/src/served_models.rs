//! The startup /v1/models fetch + cache write. The provider asks the
//! endpoint once which model ids it actually serves; the result lands in the
//! config crate's served-models cache so catalog validation can flag a stale
//! or typo'd id. Fire-and-forget at startup, all-fault-tolerant
//! (failure degrades — the caller debug-logs, the existing cache is
//! kept, nothing surfaces to the user). Existence only: the
//! OpenAI-compatible /v1/models returns ids, not capability fields.

use std::collections::HashSet;

use houyicoder_config::ServedModels;
use serde_json::Value;

/// Parse the OpenAI-compatible /v1/models response body into the served id
/// list. Deduplicates (keep first occurrence), drops blank ids, and sorts
/// longest-id-first so the substring match in config prefers the most
/// specific served id. Pure so it is unit-testable without a network.
pub fn parse_response(body: &Value) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut ids: Vec<String> = Vec::new();
    if let Some(data) = body.get("data").and_then(Value::as_array) {
        for entry in data {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                let trimmed = id.trim();
                if !trimmed.is_empty() && seen.insert(trimmed.to_lowercase()) {
                    ids.push(trimmed.to_string());
                }
            }
        }
    }
    sort_longest_first(ids)
}

/// Longest-id-first (secondary: case-insensitive lexical) so a more specific
/// served id wins the substring match in config::exists_in.
fn sort_longest_first(mut ids: Vec<String>) -> Vec<String> {
    ids.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
    ids
}

/// Write the cache atomically (temp + rename, 0o600) so a crash mid-write
/// leaves either the old cache or the new one, never a torn file.
/// Best-effort: a write failure is logged and swallowed — the cache is
/// advisory, the process still runs without it.
pub(crate) fn write_cache(path: &std::path::Path, ids: &[String]) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("served-models cache: create_dir_all failed: {e}");
        return;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let payload = ServedModels {
        ids: ids.to_vec(),
        timestamp,
    };
    let Ok(json) = serde_json::to_string(&payload) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(json.as_bytes()) {
                tracing::warn!("served-models cache: write failed: {e}");
                drop(std::fs::remove_file(&tmp));
                return;
            }
            drop(f);
            if let Err(e) = std::fs::rename(&tmp, path) {
                tracing::warn!("served-models cache: rename failed: {e}");
                drop(std::fs::remove_file(&tmp));
            }
        }
        Err(e) => tracing::warn!("served-models cache: open tmp failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_extracts_data_ids() {
        let body: Value = serde_json::json!({
            "data": [
                {"id": "qwen3.7-max", "object": "model"},
                {"id": "glm-5.2", "object": "model"},
            ]
        });
        assert_eq!(parse_response(&body), vec!["qwen3.7-max", "glm-5.2"]);
    }

    #[test]
    fn test_parse_dedups_case_insensitive() {
        let body: Value = serde_json::json!({
            "data": [{"id": "qwen3.7-max"}, {"id": "QWEN3.7-MAX"}, {"id": "glm-5.2"}]
        });
        let ids = parse_response(&body);
        assert_eq!(ids.len(), 2, "dedup case-insensitive");
        assert!(ids.contains(&"qwen3.7-max".to_string()));
    }

    #[test]
    fn test_parse_drops_blank_ids() {
        let body: Value = serde_json::json!({
            "data": [{"id": "  "}, {"object": "model"}, {"id": "glm-5.2"}]
        });
        let ids = parse_response(&body);
        assert_eq!(ids, vec!["glm-5.2"]);
    }

    #[test]
    fn test_parse_empty_without_data() {
        let body: Value = serde_json::json!({"object": "list"});
        assert!(parse_response(&body).is_empty());
    }

    #[test]
    fn test_parse_sorts_longest_id() {
        let body: Value = serde_json::json!({
            "data": [{"id": "qwen3"}, {"id": "qwen3.7-max"}, {"id": "glm"}]
        });
        let ids = parse_response(&body);
        // Longest first so the substring match prefers the most specific.
        assert_eq!(ids[0], "qwen3.7-max");
    }
}

#[cfg(test)]
mod mock_tests {
    use crate::openai_compat::OpenAiCompatibleProvider;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // A unique temp cache path per test (no env mutation — the workspace
    // denies unsafe, so set_var is out, and a path-explicit arg keeps each
    // test isolated without env races).
    fn temp_cache(slug: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("v1-mock-{slug}-{n}-{}.json", std::process::id()))
    }

    #[tokio::test]
    async fn test_refresh_writes_cache_endpoint() {
        let cache = temp_cache("write");
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "qwen3.7-max", "object": "model"},
                    {"id": "glm-5.2", "object": "model"},
                ]
            })))
            .mount(&server)
            .await;
        let provider = OpenAiCompatibleProvider::new(server.uri(), "test-key".into());
        provider
            .refresh_served_models_to(cache.clone())
            .await
            .unwrap();

        let ids = houyicoder_config::load_ids_at(&cache);
        assert_eq!(
            ids,
            vec!["qwen3.7-max", "glm-5.2"],
            "cache written from /v1/models"
        );
        drop(std::fs::remove_file(&cache));
    }

    #[tokio::test]
    async fn test_refresh_keeps_cache_error() {
        let cache = temp_cache("err");
        // Seed an existing cache so the test can assert it survives a fetch
        // failure.
        crate::served_models::write_cache(&cache, &["seed-model".into()]);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let provider = OpenAiCompatibleProvider::new(server.uri(), "test-key".into());
        let result = provider.refresh_served_models_to(cache.clone()).await;
        assert!(result.is_err(), "non-2xx returns Err, no surface");
        // The existing cache is kept (not clobbered by the failure).
        let ids = houyicoder_config::load_ids_at(&cache);
        assert_eq!(ids, vec!["seed-model"], "old cache kept on fetch failure");
        drop(std::fs::remove_file(&cache));
    }

    #[tokio::test]
    async fn test_refresh_skips_write_empty() {
        let cache = temp_cache("empty");
        crate::served_models::write_cache(&cache, &["seed-model".into()]);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": []
            })))
            .mount(&server)
            .await;
        let provider = OpenAiCompatibleProvider::new(server.uri(), "test-key".into());
        provider
            .refresh_served_models_to(cache.clone())
            .await
            .unwrap();
        // Empty list does not overwrite a good cache.
        let ids = houyicoder_config::load_ids_at(&cache);
        assert_eq!(ids, vec!["seed-model"], "empty list keeps the old cache");
        drop(std::fs::remove_file(&cache));
    }
}
