//! Tests for system route parsing and client construction.

use std::sync::Arc;

use super::SystemRoute;
use super::client_from_route;
use super::parse_system_route;
use crate::agent::tools::webfetch::resolver::PublicResolver;

const SAMPLE_OUTPUT: &str = r#"<dictionary> {
  ExceptionsList : <array> {
    0 : localhost
    1 : 127.0.0.1
    2 : *.local
    3 : 10.0.0.0/8
  }
  HTTPEnable : 1
  HTTPPort : 8118
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 1
  HTTPSPort : 8118
  HTTPSProxy : 127.0.0.1
  ProxyAutoConfigEnable : 0
}"#;

/// Both HTTP and HTTPS endpoints plus the exception list are extracted from a
/// real scutil dictionary output.
#[test]
fn test_parse_system_route_enabled() {
    let parsed = parse_system_route(SAMPLE_OUTPUT).expect("route enabled");
    assert_eq!(
        parsed,
        SystemRoute {
            http: Some("http://127.0.0.1:8118".into()),
            https: Some("http://127.0.0.1:8118".into()),
            exceptions: "localhost,127.0.0.1,*.local,10.0.0.0/8".into(),
        }
    );
    let client = client_from_route(&parsed, Arc::new(PublicResolver::new()));
    assert!(client.is_some(), "valid system route builds a client");
}

/// Disabled flags produce None so the tool uses the direct client alone.
#[test]
fn test_parse_system_route_disabled() {
    let disabled = r#"<dictionary> {
  HTTPEnable : 0
  HTTPPort : 8118
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 0
  HTTPSPort : 8118
  HTTPSProxy : 127.0.0.1
}"#;
    assert_eq!(parse_system_route(disabled), None);
}
