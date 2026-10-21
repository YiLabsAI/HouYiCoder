//! System and environment route discovery for WebFetch.
//!
//! Explicit proxy environment variables (HTTPS_PROXY, HTTP_PROXY, ALL_PROXY)
//! are treated as an operator-mandated route: when set, requests must go
//! through that route and never silently fall back to a direct connection.
//!
//! When environment variables are absent on macOS, this module queries
//! scutil lazily on the first fetch and parses HTTPProxy, HTTPSProxy, and
//! ExceptionsList. Because a macOS desktop system proxy is frequently left
//! enabled after a local proxy client is closed, a system route is tagged
//! RouteClient::System so send_hop can fall back to the direct client when
//! the local proxy port itself refuses connection, while still refusing to
//! fall back on TLS or HTTP errors.

use std::env::var_os;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_api::launcher::SpawnPolicy;
use houyicoder_api::launcher::SpawnRequest;
use houyicoder_api::launcher::StdProcessLauncher;
use pollster::block_on;
use reqwest::Client;
use reqwest::NoProxy;
use reqwest::Proxy;
use reqwest::redirect::Policy;

use super::CONNECT_TIMEOUT_SECS;
use super::FETCH_TIMEOUT_SECS;
use super::TCP_KEEPALIVE_SECS;
use super::resolver::PublicResolver;

/// A mediated HTTP client and the authority that configured it.
#[derive(Clone)]
pub(crate) enum RouteClient {
    /// Configured via explicit environment variables; never falls back to
    /// direct on failure.
    Env(Client),
    /// Discovered from macOS desktop system settings; falls back to direct
    /// only when the proxy port itself is unreachable at connect time.
    System(Client),
}

/// Parsed system route configuration from macOS scutil output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SystemRoute {
    /// HTTP endpoint URL when HTTPEnable is 1.
    pub(crate) http: Option<String>,
    /// HTTPS endpoint URL when HTTPSEnable is 1.
    pub(crate) https: Option<String>,
    /// Comma-separated bypass rules from ExceptionsList.
    pub(crate) exceptions: String,
}

const ENV_ROUTE_KEYS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// Whether any standard proxy environment variable is set and non-empty.
pub(crate) fn has_env_route() -> bool {
    ENV_ROUTE_KEYS
        .iter()
        .any(|key| var_os(key).is_some_and(|val| !val.is_empty()))
}

/// Resolve the active mediated client for a fetch. Called lazily on fetch so
/// constructing WebFetchTool never spawns a subprocess, and transient scutil
/// failures are not cached forever.
pub(crate) fn resolve_route_client(resolver: Arc<PublicResolver>) -> Option<RouteClient> {
    if has_env_route() {
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS))
            .dns_resolver(resolver)
            .build()
            .ok()?;
        return Some(RouteClient::Env(client));
    }
    let sys = cached_system_route()?;
    client_from_route(&sys, resolver).map(RouteClient::System)
}

/// Build a client from a parsed system route specification.
pub(crate) fn client_from_route(
    sys: &SystemRoute,
    resolver: Arc<PublicResolver>,
) -> Option<Client> {
    if sys.http.is_none() && sys.https.is_none() {
        return None;
    }
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS))
        .dns_resolver(resolver);
    let bypass = (!sys.exceptions.is_empty())
        .then(|| NoProxy::from_string(&sys.exceptions))
        .flatten();
    if let Some(ref url) = sys.https
        && let Ok(entry) = Proxy::https(url)
    {
        builder = builder.proxy(entry.no_proxy(bypass.clone()));
    }
    if let Some(ref url) = sys.http
        && let Ok(entry) = Proxy::http(url)
    {
        builder = builder.proxy(entry.no_proxy(bypass));
    }
    builder.build().ok()
}

fn cached_system_route() -> Option<SystemRoute> {
    static CACHE: Mutex<Option<Option<SystemRoute>>> = Mutex::new(None);
    let mut guard = CACHE.lock().ok()?;
    if let Some(ref cached) = *guard {
        return cached.clone();
    }
    let queried = query_system_route()?;
    *guard = Some(queried.clone());
    queried
}

#[cfg(target_os = "macos")]
fn query_system_route() -> Option<Option<SystemRoute>> {
    let launcher = StdProcessLauncher::new();
    let req = SpawnRequest::new("/usr/sbin/scutil")
        .with_args(["--proxy"])
        .piped_output();
    let child = launcher.spawn(req, SpawnPolicy::default()).ok()?;
    let exit = block_on(child.wait()).ok()?;
    if exit.exit_code != Some(0) {
        return None;
    }
    let text = exit.stdout?;
    Some(parse_system_route(&text))
}

#[cfg(not(target_os = "macos"))]
fn query_system_route() -> Option<Option<SystemRoute>> {
    Some(None)
}

/// Parse the dictionary output of scutil into a SystemRoute.
pub(crate) fn parse_system_route(text: &str) -> Option<SystemRoute> {
    let mut http_enable = false;
    let mut http_host: Option<String> = None;
    let mut http_port: Option<u16> = None;
    let mut https_enable = false;
    let mut https_host: Option<String> = None;
    let mut https_port: Option<u16> = None;
    let mut exceptions: Vec<String> = Vec::new();
    let mut in_exceptions = false;

    for raw in text.lines() {
        let line = raw.trim();
        if in_exceptions {
            if line == "}" {
                in_exceptions = false;
                continue;
            }
            if let Some((_, val)) = line.split_once(':') {
                let item = val.trim();
                if !item.is_empty() {
                    exceptions.push(item.to_string());
                }
            }
            continue;
        }
        if line.starts_with("ExceptionsList") && line.ends_with('{') {
            in_exceptions = true;
            continue;
        }
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "HTTPEnable" => http_enable = val == "1",
            "HTTPProxy" if !val.is_empty() => http_host = Some(val.to_string()),
            "HTTPPort" => http_port = val.parse().ok(),
            "HTTPSEnable" => https_enable = val == "1",
            "HTTPSProxy" if !val.is_empty() => https_host = Some(val.to_string()),
            "HTTPSPort" => https_port = val.parse().ok(),
            _ => {}
        }
    }

    let http = match (http_enable, http_host, http_port) {
        (true, Some(h), Some(p)) => Some(format!("http://{h}:{p}")),
        _ => None,
    };
    let https = match (https_enable, https_host, https_port) {
        (true, Some(h), Some(p)) => Some(format!("http://{h}:{p}")),
        _ => None,
    };
    if http.is_none() && https.is_none() {
        return None;
    }
    Some(SystemRoute {
        http,
        https,
        exceptions: exceptions.join(","),
    })
}

#[cfg(test)]
mod tests;
