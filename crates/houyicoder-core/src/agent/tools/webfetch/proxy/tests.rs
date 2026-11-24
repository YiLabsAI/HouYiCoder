//! Tests for the proxy client construction and the fallback gate.

use std::sync::Arc;
use std::time::Duration;

use houyicoder_platform::host::net::{
    ProxyConfig, ProxyConfigCache, ProxyResolution, ProxySnapshot, ProxySource,
};
use reqwest::Client;

use super::{FetchClient, ProxyClient, PublicResolver, build_proxy_client, loopback_only};
use crate::agent::tools::webfetch::redirect::DirectTransport;

fn snapshot_of(resolution: ProxyResolution) -> Arc<ProxySnapshot> {
    ProxyConfigCache::with_query(
        Duration::from_secs(30),
        Arc::new(move || resolution.clone()),
    )
    .snapshot()
}

fn resolver() -> Arc<PublicResolver> {
    Arc::new(PublicResolver::new())
}

/// A transport for a test proxy client. The system trust store is left out:
/// a plain-HTTP test never completes a TLS handshake, and loading the store
/// per client is what made these tests contend on the platform keychain.
fn test_client() -> Client {
    Client::builder()
        .tls_certs_only([])
        .no_proxy()
        .build()
        .expect("client")
}

/// Only a configuration whose every endpoint is on this machine counts as
/// local. One remote endpoint makes the whole proxy remote, and a scheme with
/// no endpoint contributes nothing rather than vetoing the rest.
#[test]
fn test_loopback_needs_all_local() {
    let both_local = ProxyConfig::new(
        Some("http://127.0.0.1:1".into()),
        Some("http://[::1]:1".into()),
        String::new(),
    );
    assert!(loopback_only(&both_local), "both endpoints are local");
    let http_only = ProxyConfig::new(Some("http://127.0.0.1:1".into()), None, String::new());
    assert!(loopback_only(&http_only), "the absent scheme does not veto");
    let mixed = ProxyConfig::new(
        Some("http://127.0.0.1:1".into()),
        Some("http://proxy.corp:8080".into()),
        String::new(),
    );
    assert!(
        !loopback_only(&mixed),
        "one remote endpoint makes the whole proxy remote"
    );
    let remote = ProxyConfig::new(None, Some("http://proxy.corp:8080".into()), String::new());
    assert!(!loopback_only(&remote), "a remote TLS endpoint is remote");
}

/// A loopback system proxy may fall back after a refusal.
#[test]
fn test_loopback_allows_fallback() {
    let proxy = ProxyClient::new(test_client(), ProxySource::System, true);
    assert!(
        proxy.may_fall_back(),
        "a loopback system proxy may fall back"
    );
}

/// An explicit environment proxy never falls back, even when its endpoint is
/// local.
#[test]
fn test_env_proxy_no_fallback() {
    let proxy = ProxyClient::new(test_client(), ProxySource::Env, true);
    assert!(!proxy.may_fall_back(), "an env proxy never falls back");
}

/// A system proxy on another machine never falls back.
#[test]
fn test_remote_proxy_no_fallback() {
    let proxy = ProxyClient::new(test_client(), ProxySource::System, false);
    assert!(
        !proxy.may_fall_back(),
        "a remote system proxy never falls back"
    );
}

/// A pac configuration is reported as a diagnostic rather than built, because
/// the client has no pac evaluator.
#[test]
fn test_pac_diagnostic() {
    let resolution = ProxyResolution::pac("http://corp/proxy.pac".into());
    let result = build_proxy_client(&snapshot_of(resolution), resolver());
    let message = result.expect_err("a pac configuration is reported, not built");
    assert!(
        message.contains("HTTPS_PROXY"),
        "the diagnostic names the env var to set: {message}"
    );
}

/// The direct transport is created on demand and its initialized state is
/// observable.
#[test]
fn test_direct_transport_lazy_init() {
    let client = FetchClient::new(resolver(), ProxyConfigCache::direct());
    assert!(!client.has_direct_transport());
    let _ = client.transport();
    assert!(client.has_direct_transport());
}
