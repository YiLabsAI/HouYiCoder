//! The served-model refresh over a real endpoint: the fetch writes the cache,
//! keeps a good cache when the fetch fails, and never overwrites it with an
//! empty list.
//!
//! These drive the public provider against a local HTTP endpoint, so the
//! request path is real. The unit tests beside the module state the parsing
//! and cache rules without a socket.

use houyicoder_config::load_ids_at;
use houyicoder_provider::OpenAiCompatibleProvider;
use serde_json::json;
use std::env::temp_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A unique cache path per test. The path is explicit rather than read from
/// the environment, so two tests never race on one file.
fn temp_cache(slug: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    temp_dir().join(format!("v1-mock-{slug}-{n}-{}.json", id()))
}

/// Seed a cache the way a previous successful refresh would have left it.
fn seed_cache(cache: &Path, ids: &[&str]) {
    let dir = cache.parent().expect("the cache has a parent");
    fs::create_dir_all(dir).expect("the cache dir");
    let payload = json!({ "ids": ids, "timestamp": 0 });
    fs::write(cache, serde_json::to_string(&payload).expect("json")).expect("the seed cache");
}

#[tokio::test]
async fn test_refresh_writes_cache_endpoint() {
    let cache = temp_cache("write");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
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

    let ids = load_ids_at(&cache);
    assert_eq!(
        ids,
        vec!["qwen3.7-max", "glm-5.2"],
        "cache written from /v1/models"
    );
    drop(fs::remove_file(&cache));
}

#[tokio::test]
async fn test_refresh_keeps_cache_error() {
    let cache = temp_cache("err");
    seed_cache(&cache, &["seed-model"]);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let provider = OpenAiCompatibleProvider::new(server.uri(), "test-key".into());
    let result = provider.refresh_served_models_to(cache.clone()).await;
    assert!(result.is_err(), "non-2xx returns Err, no surface");
    let ids = load_ids_at(&cache);
    assert_eq!(ids, vec!["seed-model"], "old cache kept on fetch failure");
    drop(fs::remove_file(&cache));
}

#[tokio::test]
async fn test_refresh_skips_write_empty() {
    let cache = temp_cache("empty");
    seed_cache(&cache, &["seed-model"]);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .mount(&server)
        .await;
    let provider = OpenAiCompatibleProvider::new(server.uri(), "test-key".into());
    provider
        .refresh_served_models_to(cache.clone())
        .await
        .unwrap();
    let ids = load_ids_at(&cache);
    assert_eq!(ids, vec!["seed-model"], "empty list keeps the old cache");
    drop(fs::remove_file(&cache));
}
