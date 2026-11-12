//! Tests for the platform-agnostic proxy model, parsers, and cache.
//!
//! Every case runs on every target: the parsers handle the host and port form
//! and the semicolon key-value form the desktop settings share, so no test
//! names a platform.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;

#[test]
fn test_bare_endpoint() {
    let (http, https) = parse_proxy_endpoints("proxy.corp:8080").expect("a bare endpoint");
    assert_eq!(http.as_deref(), Some("http://proxy.corp:8080"));
    assert_eq!(https.as_deref(), Some("http://proxy.corp:8080"));
}

#[test]
fn test_per_scheme_endpoints() {
    let (http, https) =
        parse_proxy_endpoints("http=proxy.corp:8080; https=secure.corp:8443").expect("a list");
    assert_eq!(http.as_deref(), Some("http://proxy.corp:8080"));
    assert_eq!(https.as_deref(), Some("http://secure.corp:8443"));
}

#[test]
fn test_local_bypass_expansion() {
    let rules = expand_bypass_rules("<local>;*.corp;10.0.0.0/8");
    assert_eq!(rules, "localhost,127.0.0.1,::1,*.corp,10.0.0.0/8");
}

#[test]
fn test_env_proxy_tls_only() {
    let settings = EnvProxySettings {
        https: Some("http://proxy:8080".into()),
        http: None,
        all: None,
        no_proxy: None,
    };
    let proxy = proxy_from_env(&settings).expect("an env proxy");
    assert_eq!(proxy.source, ProxySource::Env);
    let config = proxy.proxy.expect("a config");
    assert_eq!(config.http, None);
    assert_eq!(config.https.as_deref(), Some("http://proxy:8080"));
}

#[test]
fn test_env_proxy_all_schemes() {
    let settings = EnvProxySettings {
        https: None,
        http: None,
        all: Some("http://proxy:8080".into()),
        no_proxy: None,
    };
    let proxy = proxy_from_env(&settings).expect("an env proxy");
    let config = proxy.proxy.expect("a config");
    assert_eq!(config.http.as_deref(), Some("http://proxy:8080"));
    assert_eq!(config.https.as_deref(), Some("http://proxy:8080"));
}

#[test]
fn test_env_proxy_bypass_rules() {
    let settings = EnvProxySettings {
        https: Some("http://proxy:8080".into()),
        http: None,
        all: None,
        no_proxy: Some("127.0.0.1,localhost".into()),
    };
    let proxy = proxy_from_env(&settings).expect("an env proxy");
    let config = proxy.proxy.expect("a config");
    assert_eq!(config.exceptions, "127.0.0.1,localhost");
}

#[test]
fn test_env_proxy_blank_ignored() {
    let settings = EnvProxySettings {
        https: Some("   ".into()),
        http: None,
        all: None,
        no_proxy: None,
    };
    assert!(
        proxy_from_env(&settings).is_none(),
        "a blank value is no proxy"
    );
}

#[test]
fn test_cache_reuses_within_lifetime() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let cache = ProxyConfigCache::with_query(
        Duration::from_secs(30),
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            ProxyResolution::direct()
        }),
    );
    let now = Instant::now();
    cache.snapshot_at(now);
    cache.snapshot_at(now + Duration::from_secs(10));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the system proxy is read once inside the lifetime"
    );
}

#[test]
fn test_cache_refreshes_after_lifetime() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let cache = ProxyConfigCache::with_query(
        Duration::from_secs(30),
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            ProxyResolution::direct()
        }),
    );
    let now = Instant::now();
    cache.snapshot_at(now);
    cache.snapshot_at(now + Duration::from_secs(31));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the system proxy is read again past the lifetime"
    );
}

#[test]
fn test_cache_generation_stable() {
    let cache = ProxyConfigCache::with_query(
        Duration::from_secs(30),
        Arc::new(|| {
            ProxyResolution::system(ProxyConfig::new(
                Some("http://proxy:8080".into()),
                Some("http://proxy:8080".into()),
                String::new(),
            ))
        }),
    );
    let now = Instant::now();
    let first = cache.snapshot_at(now);
    let second = cache.snapshot_at(now + Duration::from_secs(31));
    assert_eq!(
        first.generation(),
        second.generation(),
        "an unchanged proxy keeps the generation"
    );
}

#[test]
fn test_cache_generation_rises() {
    let current = Arc::new(Mutex::new(ProxyResolution::direct()));
    let snapshot = Arc::clone(&current);
    let cache = ProxyConfigCache::with_query(
        Duration::from_secs(30),
        Arc::new(move || snapshot.lock().unwrap().clone()),
    );
    let now = Instant::now();
    let first = cache.snapshot_at(now);
    *current.lock().unwrap() = ProxyResolution::system(ProxyConfig::new(
        Some("http://proxy:8080".into()),
        Some("http://proxy:8080".into()),
        String::new(),
    ));
    let second = cache.snapshot_at(now + Duration::from_secs(31));
    assert!(
        second.generation() > first.generation(),
        "a changed proxy raises the generation"
    );
}

#[test]
fn test_loopback_endpoint_matrix() {
    assert!(is_loopback_endpoint("http://127.0.0.1:8080"));
    assert!(is_loopback_endpoint("http://localhost:8080"));
    assert!(is_loopback_endpoint("http://[::1]:8080"));
    assert!(!is_loopback_endpoint("http://proxy.corp:8080"));
    assert!(!is_loopback_endpoint("http://10.0.0.1:8080"));
}

#[test]
fn test_redacted_endpoint_hides_secret() {
    assert_eq!(
        redact_endpoint("http://alice:S3cr3t@proxy.corp:8080"),
        "http://proxy.corp:8080"
    );
    assert_eq!(redact_endpoint("127.0.0.1:8080"), "127.0.0.1:8080");
    assert_eq!(
        redact_endpoint("http://proxy.corp:8080"),
        "http://proxy.corp:8080"
    );
    assert_eq!(
        redact_endpoint("http://alice:secret@host:80/path"),
        "http://host:80/path"
    );
}

#[test]
fn test_proxy_config_debug_redacts() {
    let config = ProxyConfig::new(
        Some("http://alice:S3cr3t@proxy:8080".into()),
        Some("http://bob:hunter2@secure:8443".into()),
        String::new(),
    );
    let formatted = format!("{config:?}");
    assert!(
        !formatted.contains("S3cr3t") && !formatted.contains("hunter2"),
        "Debug must not leak passwords: {formatted}"
    );
    assert!(
        !formatted.contains("alice") && !formatted.contains("bob"),
        "Debug must not leak users: {formatted}"
    );
    assert!(
        formatted.contains("proxy:8080") && formatted.contains("secure:8443"),
        "Debug keeps the hosts: {formatted}"
    );
}

#[test]
fn test_single_flight_serves_stale() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let query: ProxyQuery = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        let _ = release_rx.lock().unwrap().recv();
        ProxyResolution::system(ProxyConfig::new(
            Some("http://127.0.0.1:1".into()),
            Some("http://127.0.0.1:1".into()),
            String::new(),
        ))
    });
    let cache = ProxyConfigCache::with_query(Duration::from_secs(30), query);
    let now = Instant::now();

    let cache_cold = Arc::clone(&cache);
    let cold = thread::spawn(move || cache_cold.snapshot_at(now));
    while calls.load(Ordering::SeqCst) == 0 {
        thread::yield_now();
    }
    release_tx.send(()).unwrap();
    let first = cold.join().unwrap();
    assert_eq!(first.source(), ProxySource::System);

    let expired = now + Duration::from_secs(31);
    let cache_refresh = Arc::clone(&cache);
    let refresh = thread::spawn(move || cache_refresh.snapshot_at(expired));
    while calls.load(Ordering::SeqCst) == 1 {
        thread::yield_now();
    }

    let cache_stale = Arc::clone(&cache);
    let stale = cache_stale.snapshot_at(expired);
    assert_eq!(
        stale.generation(),
        first.generation(),
        "a second caller serves stale while one refreshes"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "only one refresh query runs for concurrent callers"
    );

    release_tx.send(()).unwrap();
    let refreshed = refresh.join().unwrap();
    assert_eq!(refreshed.source(), ProxySource::System);
}

/// A query that panics during a refresh must not leave the cache stuck: the
/// next refresh past the lifetime runs the query again rather than serving
/// stale with the refreshing flag held true.
#[test]
fn test_refresh_after_query_panic() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let query: ProxyQuery = Arc::new(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n == 1 {
            panic!("a query failure during refresh");
        }
        ProxyResolution::direct()
    });
    let cache = ProxyConfigCache::with_query(Duration::from_secs(30), query);
    let t0 = Instant::now();
    cache.snapshot_at(t0);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let expired = t0 + Duration::from_secs(31);
    let panicked = catch_unwind(AssertUnwindSafe(|| cache.snapshot_at(expired)));
    assert!(panicked.is_err(), "the second call panics inside the query");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let later = t0 + Duration::from_secs(62);
    cache.snapshot_at(later);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "the panic reset the refreshing flag so the next refresh runs"
    );
}
