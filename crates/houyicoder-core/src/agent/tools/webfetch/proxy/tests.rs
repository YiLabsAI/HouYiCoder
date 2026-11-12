//! Tests for the proxy client construction and the fallback gate.

use std::sync::Arc;
use std::time::Duration;

use houyicoder_platform::host::net::{
    ProxyConfig, ProxyConfigCache, ProxyResolution, ProxySnapshot,
};

use super::{PublicResolver, build_proxy_client};

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

#[test]
fn test_loopback_allows_fallback() {
    let resolution = ProxyResolution::system(ProxyConfig::new(
        Some("http://127.0.0.1:1".into()),
        Some("http://127.0.0.1:1".into()),
        String::new(),
    ));
    let proxy = build_proxy_client(&snapshot_of(resolution), resolver())
        .expect("a loopback proxy builds")
        .expect("a proxy client");
    assert!(
        proxy.may_fall_back(),
        "a loopback system proxy may fall back"
    );
}

#[test]
fn test_env_proxy_no_fallback() {
    let resolution = ProxyResolution::env(ProxyConfig::new(
        Some("http://127.0.0.1:1".into()),
        Some("http://127.0.0.1:1".into()),
        String::new(),
    ));
    let proxy = build_proxy_client(&snapshot_of(resolution), resolver())
        .expect("an env proxy builds")
        .expect("a proxy client");
    assert!(!proxy.may_fall_back(), "an env proxy never falls back");
}

#[test]
fn test_remote_proxy_no_fallback() {
    let resolution = ProxyResolution::system(ProxyConfig::new(
        Some("http://proxy.corp:8080".into()),
        Some("http://proxy.corp:8080".into()),
        String::new(),
    ));
    let proxy = build_proxy_client(&snapshot_of(resolution), resolver())
        .expect("a remote proxy builds")
        .expect("a proxy client");
    assert!(
        !proxy.may_fall_back(),
        "a remote system proxy never falls back"
    );
}

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
