//! Tests for the screening resolver.

use std::net::SocketAddr;

use reqwest::dns::Name;
use reqwest::dns::Resolve;

use super::PRIVATE_TARGET;
use super::PublicResolver;

fn addr(text: &str) -> SocketAddr {
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
    let resolver = PublicResolver::with_fixed(vec![addr("93.184.216.34:0")]);
    assert_eq!(
        resolve_once(&resolver).expect("public answer"),
        vec![addr("93.184.216.34:0")]
    );
}

/// A loopback answer is refused at the lookup the connection would use, so no
/// socket is opened to it.
#[test]
fn test_private_answer_refused() {
    for private in ["127.0.0.1:0", "[::1]:0", "10.1.2.3:0", "169.254.1.1:0"] {
        let resolver = PublicResolver::with_fixed(vec![addr(private)]);
        let error = resolve_once(&resolver).expect_err("private answer is refused");
        assert!(
            error.contains(PRIVATE_TARGET),
            "the refusal names the screen: {error}"
        );
    }
}

/// One answer is refused whole: a name that returns a public address beside a
/// private one is not usable.
#[test]
fn test_mixed_answer_refused() {
    let resolver = PublicResolver::with_fixed(vec![addr("93.184.216.34:0"), addr("127.0.0.1:0")]);
    let error = resolve_once(&resolver).expect_err("a mixed answer is refused");
    assert!(
        error.contains(PRIVATE_TARGET),
        "the refusal names it: {error}"
    );
}

/// Each call performs exactly one lookup, which is what leaves no room for a
/// second answer to disagree with the screen.
#[test]
fn test_one_lookup_per_call() {
    let resolver = PublicResolver::with_fixed(vec![addr("93.184.216.34:0")]);
    assert_eq!(resolver.lookup_count(), 0);
    resolve_once(&resolver).expect("answer");
    assert_eq!(resolver.lookup_count(), 1);
    resolve_once(&resolver).expect("answer");
    assert_eq!(resolver.lookup_count(), 2);
}
