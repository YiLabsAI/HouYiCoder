//! Tests for the Windows proxy resolution logic.

use crate::host::net::{ProxyDiagnostic, ProxyResolution, ProxySource};

use super::build_resolution;

/// A default install has auto-detection on but no static proxy and no script
/// URL. Browsers connect direct on a network with no WPAD server, and so must
/// this client: treating auto-detection alone as an error would block every
/// default machine.
#[test]
fn test_default_install_direct() {
    let resolution = build_resolution(None, None, None);
    assert_eq!(resolution, ProxyResolution::direct());
}

/// An empty script URL is treated as no script, so the client connects direct
/// rather than reporting an empty automatic-configuration address.
#[test]
fn test_empty_script_is_direct() {
    let resolution = build_resolution(None, None, Some(String::new()));
    assert_eq!(resolution, ProxyResolution::direct());
}

/// An explicit automatic-configuration script the client cannot evaluate is
/// reported with the way out, not silently treated as direct.
#[test]
fn test_pac_script_reported() {
    let resolution = build_resolution(None, None, Some("http://corp/wpad.dat".into()));
    assert!(resolution.proxy.is_none());
    match resolution.diagnostic {
        Some(ProxyDiagnostic::PacUnsupported { auto_config_url }) => {
            assert_eq!(auto_config_url, "http://corp/wpad.dat");
        }
        other => panic!("a script-only config is a diagnostic, got {other:?}"),
    }
}

/// A static proxy wins even when an automatic-configuration script is also set.
#[test]
fn test_static_proxy_wins() {
    let resolution = build_resolution(
        Some("proxy.corp:8080".into()),
        None,
        Some("http://corp/wpad.dat".into()),
    );
    assert_eq!(resolution.source, ProxySource::System);
    let config = resolution.proxy.expect("a static proxy wins");
    assert_eq!(config.http.as_deref(), Some("http://proxy.corp:8080"));
    assert_eq!(config.https.as_deref(), Some("http://proxy.corp:8080"));
}
