//! Tests for this module.

use super::*;

/// An owner that was never handed a resolution reports an endpoint, and labels
/// the state so a forgotten attach cannot read as an authoritative
/// environment-derived answer.
#[test]
fn test_unresolved_is_labelled() {
    let facts = ProviderFacts::unresolved();
    assert!(!facts.base_url.is_empty(), "the environment names a host");
    assert_eq!(facts.key_origin.as_deref(), Some("unresolved"));
}

/// The stub carries no endpoint, so an owner that reports it must fall back
/// rather than render an empty host.
#[test]
fn test_stub_carries_no_endpoint() {
    let resolution = ProviderResolution::stub();
    assert_eq!(resolution.facts, ProviderFacts::default());
    assert!(resolution.warnings.is_empty());
}
