//! Tests for the screening resolver and single-lookup address reuse.

use std::net::SocketAddr;

use reqwest::dns::Name;
use reqwest::dns::Resolve;
use tokio::runtime::Builder;

use super::PRIVATE_TARGET;
use super::PublicResolver;

fn parse_target(text: &str) -> SocketAddr {
    text.parse().expect("address parses")
}

fn resolve_once(resolver: &PublicResolver) -> Result<Vec<SocketAddr>, String> {
    let name: Name = "example.test".parse().expect("name parses");
    let addrs = pollster::block_on(resolver.resolve(name)).map_err(|e| e.to_string())?;
    Ok(addrs.collect())
}

/// A public answer is handed to the connection unchanged.
#[test]
fn test_public_answer_passes() {
    let resolver = PublicResolver::with_fixed(vec![parse_target("93.184.216.34:0")]);
    assert_eq!(
        resolve_once(&resolver).expect("public answer"),
        vec![parse_target("93.184.216.34:0")]
    );
}

/// A loopback or private answer is refused by both screen_target and resolve.
#[test]
fn test_private_answer_refused() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    for private in ["127.0.0.1:80", "[::1]:80", "10.1.2.3:80", "169.254.1.1:80"] {
        let resolver = PublicResolver::with_fixed(vec![parse_target(private)]);
        let err = runtime
            .block_on(resolver.screen_target("https://example.test/x"))
            .expect_err("screen_target refuses private IP");
        assert_eq!(err["error_kind"], "private_target");
        let direct_err = resolve_once(&resolver).expect_err("resolve refuses private IP");
        assert!(
            direct_err.contains(PRIVATE_TARGET),
            "the refusal names the screen: {direct_err}"
        );
    }
}

/// One answer is refused whole: a name that returns a public address beside a
/// private one is not usable.
#[test]
fn test_mixed_answer_refused() {
    let resolver = PublicResolver::with_fixed(vec![
        parse_target("93.184.216.34:0"),
        parse_target("127.0.0.1:0"),
    ]);
    let error = resolve_once(&resolver).expect_err("a mixed answer is refused");
    assert!(
        error.contains(PRIVATE_TARGET),
        "the refusal names it: {error}"
    );
}

/// When screen_target validates a public IP first and a second DNS answer
/// flips to 127.0.0.1, the connector receives the held public IP from the
/// first lookup and performs zero additional OS lookups.
#[test]
fn test_reuses_first_lookup() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let resolver = PublicResolver::with_sequence(vec![
        vec![parse_target("93.184.216.34:443")],
        vec![parse_target("127.0.0.1:443")],
    ]);
    runtime
        .block_on(resolver.screen_target("https://example.test/path"))
        .expect("first lookup is public");
    assert_eq!(resolver.lookup_count(), 1, "screen_target did 1 lookup");

    let connected = resolve_once(&resolver).expect("connector reuses held address");
    assert_eq!(
        connected,
        vec![parse_target("93.184.216.34:443")],
        "connector must use the held public IP, never the rebound 127.0.0.1"
    );
    assert_eq!(
        resolver.lookup_count(),
        1,
        "connector reuse must not trigger a second lookup"
    );
}
