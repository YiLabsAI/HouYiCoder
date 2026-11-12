//! Proxy policy of the model client: an explicit or system proxy that cannot
//! carry the request fails it, and only a change in the loaded configuration
//! rebuilds the client. The mock server stands in for the model endpoint and
//! records every request that reached it directly.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use houyicoder_api::provider::ModelProvider;
use houyicoder_platform::host::net::{ProxyConfig, ProxyConfigCache, ProxyResolution};
use houyicoder_protocol::cache_policy::PromptCacheFormat;
use houyicoder_protocol::llm::{CompletionRequest, InputItem, ModelSettings, ProviderError};
use houyicoder_provider::OpenAiCompatibleProvider;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "test-model".into(),
        instructions: "you are a test agent".into(),
        dynamic_boundary: None,
        input: vec![InputItem::User {
            content: "hello".into(),
        }],
        tools: vec![],
        settings: ModelSettings::default(),
        cache_breakpoints: Vec::new(),
        cache_format: PromptCacheFormat::AutoPrefix,
    }
}

fn cache_with(resolution: ProxyResolution) -> Arc<ProxyConfigCache> {
    ProxyConfigCache::with_query(
        Duration::from_secs(60),
        Arc::new(move || resolution.clone()),
    )
}

fn closed_proxy() -> ProxyConfig {
    ProxyConfig::new(
        Some("http://127.0.0.1:1".into()),
        Some("http://127.0.0.1:1".into()),
        String::new(),
    )
}

async fn model_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"choices": [{"message": {"content": "ok"}}]})),
        )
        .mount(&server)
        .await;
    server
}

/// A reached model endpoint answers with content, which is the positive
/// control the failure cases are read against: no proxy means the provider
/// connects direct, including to a loopback model gateway.
#[tokio::test]
async fn test_direct_loopback_gateway() {
    let server = model_server().await;
    let provider = OpenAiCompatibleProvider::with_proxy_cache(
        server.uri(),
        "k".into(),
        cache_with(ProxyResolution::direct()),
    );
    let response = provider
        .complete(request())
        .await
        .expect("a direct provider reaches the loopback endpoint");
    assert_eq!(response.model, "test-model");
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        1,
        "the direct path sends exactly one request"
    );
}

/// An explicit environment proxy that refuses the connection fails the
/// request; the model endpoint records no direct request.
#[tokio::test]
async fn test_env_proxy_closed_errors() {
    let server = model_server().await;
    let provider = OpenAiCompatibleProvider::with_proxy_cache(
        server.uri(),
        "k".into(),
        cache_with(ProxyResolution::env(closed_proxy())),
    );
    let error = provider
        .complete(request())
        .await
        .expect_err("the closed env proxy fails the request");
    assert!(
        matches!(error, ProviderError::Network),
        "the failure is a transport error: {error}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "an env proxy never bypasses to a direct connection"
    );
}

/// A system proxy that refuses the connection fails the request too, even
/// when the proxy is on this machine: the model path has no fallback at all.
#[tokio::test]
async fn test_system_proxy_closed_errors() {
    let server = model_server().await;
    let provider = OpenAiCompatibleProvider::with_proxy_cache(
        server.uri(),
        "k".into(),
        cache_with(ProxyResolution::system(closed_proxy())),
    );
    provider
        .complete(request())
        .await
        .expect_err("a closed system proxy fails the request");
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "a system proxy never falls back to a direct connection"
    );
}

/// An automatic configuration the client cannot evaluate is reported with the
/// way out, and the request is never sent direct.
#[tokio::test]
async fn test_pac_proxy_reported() {
    let server = model_server().await;
    let provider = OpenAiCompatibleProvider::with_proxy_cache(
        server.uri(),
        "k".into(),
        cache_with(ProxyResolution::pac("http://wpad/wpad.dat".into())),
    );
    let error = provider
        .complete(request())
        .await
        .expect_err("an automatic configuration fails the request");
    let message = error.to_string();
    assert!(
        message.contains("HTTPS_PROXY"),
        "the message names the actionable alternative: {message}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "the request was not sent direct"
    );
}

/// A changed configuration raises the generation, and the next request uses a
/// client rebuilt for it rather than the one the closed proxy produced.
#[tokio::test]
async fn test_proxy_change_recovers() {
    let server = model_server().await;
    let current = Arc::new(Mutex::new(ProxyResolution::system(closed_proxy())));
    let source = Arc::clone(&current);
    let cache = ProxyConfigCache::with_query(
        Duration::ZERO,
        Arc::new(move || {
            source
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }),
    );
    let provider = OpenAiCompatibleProvider::with_proxy_cache(server.uri(), "k".into(), cache);
    provider
        .complete(request())
        .await
        .expect_err("the closed proxy fails the first request");
    *current
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = ProxyResolution::direct();
    let response = provider
        .complete(request())
        .await
        .expect("the rebuilt direct client reaches the endpoint");
    assert_eq!(response.model, "test-model");
}

/// A TLS-only proxy configuration pointing at a closed port.
fn closed_https_proxy() -> ProxyConfig {
    ProxyConfig::new(None, Some("http://127.0.0.1:1".into()), String::new())
}

/// A proxy set for TLS only sends a plain HTTP target direct by the standard
/// semantics, so a local model endpoint stays reachable when only the TLS
/// proxy is configured.
#[tokio::test]
async fn test_https_proxy_plain_target() {
    let server = model_server().await;
    let provider = OpenAiCompatibleProvider::with_proxy_cache(
        server.uri(),
        "k".into(),
        cache_with(ProxyResolution::env(closed_https_proxy())),
    );
    let response = provider
        .complete(request())
        .await
        .expect("a plain endpoint with a TLS-only proxy goes direct");
    assert_eq!(response.model, "test-model");
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        1,
        "the plain target reached the endpoint direct"
    );
}

/// A TLS-only proxy that is closed must fail a TLS target rather than bypass
/// the proxy: the endpoint records no direct request.
#[tokio::test]
async fn test_https_proxy_tls_strict() {
    let server = model_server().await;
    let https_uri = server.uri().replace("http://", "https://");
    let provider = OpenAiCompatibleProvider::with_proxy_cache(
        https_uri,
        "k".into(),
        cache_with(ProxyResolution::env(closed_https_proxy())),
    );
    provider
        .complete(request())
        .await
        .expect_err("a closed TLS proxy fails the TLS target");
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "a closed TLS proxy never bypasses to a direct connection"
    );
}
