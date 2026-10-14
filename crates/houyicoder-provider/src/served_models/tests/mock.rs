use crate::openai_compat::OpenAiCompatibleProvider;
use crate::served_models::write_cache;
use std::env::temp_dir;
use std::fs::remove_file;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// A unique temp cache path per test (no env mutation — the workspace
// denies unsafe, so set_var is out, and a path-explicit arg keeps each
// test isolated without env races).
fn temp_cache(slug: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    temp_dir().join(format!("v1-mock-{slug}-{n}-{}.json", id()))
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
    drop(remove_file(&cache));
}

#[tokio::test]
async fn test_refresh_keeps_cache_error() {
    let cache = temp_cache("err");
    // Seed an existing cache so the test can assert it survives a fetch
    // failure.
    write_cache(&cache, &["seed-model".into()]);
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
    drop(remove_file(&cache));
}

#[tokio::test]
async fn test_refresh_skips_write_empty() {
    let cache = temp_cache("empty");
    write_cache(&cache, &["seed-model".into()]);
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
    drop(remove_file(&cache));
}
