//! Tests for the platform-agnostic proxy model, parsers, and cache.
//!
//! Every case runs on every target: the parsers handle the host and port form
//! and the semicolon key-value form the desktop settings share, so no test
//! names a platform.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::cache::REFRESH_BOUND;
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

/// Wait until the query has run the given number of times. The wait polls the
/// counter the query raises, so it ends as soon as the read runs and fails
/// rather than hanging when the read never starts.
fn wait_for_calls(calls: &AtomicU32, count: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) < count {
        assert!(Instant::now() < deadline, "the query ran {count} times");
        thread::yield_now();
    }
}

/// Wait for a background read to replace the snapshot, failing rather than
/// hanging when it never does.
fn wait_for_generation(cache: &ProxyConfigCache, above: u64) -> Arc<ProxySnapshot> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = cache.snapshot_at(Instant::now());
        if snapshot.generation() > above {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "the background read replaced the snapshot"
        );
        thread::yield_now();
    }
}

/// A caller past the lifetime gets the snapshot already held, and the read
/// that replaces it runs behind the call.
#[test]
fn test_expired_snapshot_kept() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let query: ProxyQuery = Arc::new(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            ProxyResolution::direct()
        } else {
            ProxyResolution::system(ProxyConfig::new(
                Some("http://proxy:8080".into()),
                Some("http://proxy:8080".into()),
                String::new(),
            ))
        }
    });
    let cache = ProxyConfigCache::with_query(Duration::from_secs(30), query);
    let now = Instant::now();
    let first = cache.snapshot_at(now);
    let stale = cache.snapshot_at(now + Duration::from_secs(31));
    assert!(
        stale.proxy().is_none(),
        "the call past the lifetime returns the snapshot already held"
    );
    assert_eq!(stale.generation(), first.generation());
    let refreshed = wait_for_generation(&cache, first.generation());
    assert_eq!(
        refreshed.source(),
        ProxySource::System,
        "the read behind the call replaces the snapshot"
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
    let second = cache.snapshot_at(now + Duration::from_secs(10));
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
    let stale = cache.snapshot_at(now + Duration::from_secs(31));
    assert_eq!(
        stale.generation(),
        first.generation(),
        "the expired call returns the snapshot already held"
    );
    let second = wait_for_generation(&cache, first.generation());
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

/// Two callers past the lifetime start one read between them: the first gets
/// the snapshot already held and starts the read, and the second finds that
/// read in flight and gets the same snapshot.
#[test]
fn test_single_flight_reuses_stale() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let query: ProxyQuery = Arc::new(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n > 0 {
            // A read past the first waits for the test, so the window a
            // second caller lands in stays open for as long as it takes.
            let _ = release_rx.lock().unwrap().recv();
        }
        ProxyResolution::system(ProxyConfig::new(
            Some(format!("http://127.0.0.1:{}", n + 1)),
            Some(format!("http://127.0.0.1:{}", n + 1)),
            String::new(),
        ))
    });
    let cache = ProxyConfigCache::with_query(Duration::from_secs(30), query);
    let now = Instant::now();
    let first = cache.snapshot_at(now);
    assert_eq!(first.source(), ProxySource::System);

    let expired = now + Duration::from_secs(31);
    let stale = cache.snapshot_at(expired);
    assert_eq!(
        stale.generation(),
        first.generation(),
        "the expired call returns the snapshot already held"
    );
    wait_for_calls(&calls, 2);

    let stale_again = cache.snapshot_at(expired);
    assert_eq!(
        stale_again.generation(),
        first.generation(),
        "a second caller gets the same snapshot while a read runs"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "only one refresh query runs for concurrent callers"
    );

    release_tx.send(()).unwrap();
    let refreshed = wait_for_generation(&cache, first.generation());
    assert_eq!(
        refreshed.proxy().and_then(|proxy| proxy.http.as_deref()),
        Some("http://127.0.0.1:2"),
        "the read started by the first caller replaces the snapshot"
    );
}

/// A read that never returns stops holding the refresh slot after its bound,
/// so a settings call that hangs costs freshness rather than freezing the
/// cache.
#[test]
fn test_refresh_recovers_after_hang() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let query: ProxyQuery = Arc::new(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        if n > 0 {
            let _ = release_rx.lock().unwrap().recv();
        }
        ProxyResolution::direct()
    });
    let cache = ProxyConfigCache::with_query(Duration::from_secs(30), query);
    let now = Instant::now();
    cache.snapshot_at(now);
    let expired = now + Duration::from_secs(31);
    cache.snapshot_at(expired);
    wait_for_calls(&calls, 2);

    // The read that never returned held the slot past its bound, so a later
    // caller starts another instead of keeping stale forever.
    cache.snapshot_at(expired + REFRESH_BOUND + Duration::from_secs(1));
    wait_for_calls(&calls, 3);
    release_tx.send(()).unwrap();
    release_tx.send(()).unwrap();
}

/// A query that panics during a refresh must not leave the cache stuck: the
/// next refresh past the lifetime runs the query again rather than keeping
/// stale with the refresh slot held.
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
    // The failed read unwinds on its own thread, so the caller that started
    // it gets the snapshot already held and never sees the panic.
    let expired = t0 + Duration::from_secs(31);
    cache.snapshot_at(expired);
    wait_for_calls(&calls, 2);
    // The failed read releases the refresh slot as it unwinds, which can
    // trail the counter this test waits on, so the next caller is tried until
    // it finds the slot free and starts the read.
    let later = t0 + Duration::from_secs(62);
    let deadline = Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) < 3 {
        cache.snapshot_at(later);
        assert!(
            Instant::now() < deadline,
            "the panic released the refresh slot so the next refresh runs"
        );
        thread::yield_now();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

/// A lifetime of zero is a caller asking for no reuse, so every call reads on
/// the calling thread.
#[test]
fn test_zero_lifetime_reads() {
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let cache = ProxyConfigCache::with_query(
        Duration::ZERO,
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            ProxyResolution::direct()
        }),
    );
    let now = Instant::now();
    cache.snapshot_at(now);
    cache.snapshot_at(now);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a zero lifetime reads on every call"
    );
}

/// A background read that started before another one and finishes after it
/// does not replace the newer snapshot: the newer read already answered.
#[test]
fn test_late_refresh_discarded() {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let calls = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&calls);
    let query: ProxyQuery = Arc::new(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        match n {
            0 => ProxyResolution::direct(),
            1 => {
                // The older read is held until the newer one has landed, so
                // it returns second with the answer it read first.
                let _ = release_rx.lock().unwrap().recv();
                ProxyResolution::direct()
            }
            _ => ProxyResolution::system(ProxyConfig::new(
                Some("http://proxy:8080".into()),
                Some("http://proxy:8080".into()),
                String::new(),
            )),
        }
    });
    let cache = ProxyConfigCache::with_query(Duration::from_secs(30), query);
    let now = Instant::now();
    let first = cache.snapshot_at(now);
    let expired = now + Duration::from_secs(31);
    cache.snapshot_at(expired);
    wait_for_calls(&calls, 2);

    let rearmed = expired + REFRESH_BOUND + Duration::from_secs(1);
    cache.snapshot_at(rearmed);
    let newer = wait_for_generation(&cache, first.generation());
    assert_eq!(
        newer.source(),
        ProxySource::System,
        "the read that took the slot past its bound replaced the snapshot"
    );

    release_tx.send(()).unwrap();
    // The older read returns after the newer snapshot landed; the window
    // gives it time to finish and fails the moment it applies.
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        assert_eq!(
            cache.snapshot_at(Instant::now()).generation(),
            newer.generation(),
            "a read that started earlier does not replace the newer snapshot"
        );
        thread::yield_now();
    }
}
