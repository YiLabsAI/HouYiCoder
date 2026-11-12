//! Tests for the macOS probe dictionary parser.

use crate::host::net::{ProxyDiagnostic, ProxyResolution, ProxySource};

use super::parse_scutil_proxy;

#[test]
fn test_scutil_proxy_enabled() {
    let text = "\
<dictionary> {
  HTTPEnable : 1
  HTTPProxy : 127.0.0.1
  HTTPPort : 8080
  HTTPSEnable : 1
  HTTPSProxy : 127.0.0.1
  HTTPSPort : 8080
  ExceptionsList : <array> {
    0 : 10.0.0.0/8
    1 : fe80::1
    2 : *.local
  }
}
";
    let proxy = parse_scutil_proxy(text);
    assert_eq!(proxy.source, ProxySource::System);
    let config = proxy.proxy.expect("a static proxy");
    assert_eq!(config.http.as_deref(), Some("http://127.0.0.1:8080"));
    assert_eq!(config.https.as_deref(), Some("http://127.0.0.1:8080"));
    assert_eq!(config.exceptions, "10.0.0.0/8,fe80::1,*.local");
}

#[test]
fn test_scutil_proxy_pac_only() {
    let text = "\
ProxyAutoConfigEnable : 1
ProxyAutoConfigURLString : http://corp/proxy.pac
";
    let proxy = parse_scutil_proxy(text);
    assert!(proxy.proxy.is_none());
    match proxy.diagnostic {
        Some(ProxyDiagnostic::PacUnsupported { auto_config_url }) => {
            assert_eq!(auto_config_url, "http://corp/proxy.pac");
        }
        other => panic!("a pac-only config is a diagnostic, got {other:?}"),
    }
}

#[test]
fn test_scutil_proxy_disabled() {
    let text = "\
HTTPEnable : 0
HTTPSEnable : 0
ProxyAutoConfigEnable : 0
";
    assert_eq!(parse_scutil_proxy(text), ProxyResolution::direct());
}
