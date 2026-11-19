//! Proxy discovery shared by model and web traffic.
//!
//! Explicit proxy environment variables win; otherwise the desktop system
//! settings are read by a per-target reader in this crate. Automatic
//! configuration alone is a diagnostic, never a silent direct connection.
//! Each consumer builds its own client from a credential-free snapshot, and
//! the generation rises only when the resolution changes.

mod cache;

use std::env::var;
use std::fmt::{self, Formatter};
use std::net::IpAddr;
use std::time::Duration;

pub use self::cache::{ProxyConfigCache, ProxyQuery};

/// How long a resolved proxy configuration is reused before the system
/// settings are read again.
pub const PROXY_CONFIG_TTL: Duration = Duration::from_secs(30);

/// A proxy configuration without any credential: the endpoints per scheme and
/// the bypass list. Diagnostic and log output redacts credentials out of the
/// endpoint strings, so a password embedded in a proxy URL never reaches the
/// model context or a persisted log.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    /// Endpoint for plain HTTP destinations, when the configuration names one.
    pub http: Option<String>,
    /// Endpoint for TLS destinations, when the configuration names one.
    pub https: Option<String>,
    /// Bypass rules in the comma-separated form the HTTP client accepts.
    pub exceptions: String,
}

impl ProxyConfig {
    /// A configuration over the given endpoints and bypass rules.
    pub fn new(http: Option<String>, https: Option<String>, exceptions: String) -> Self {
        Self {
            http,
            https,
            exceptions,
        }
    }

    /// The endpoint for a scheme, when the configuration names one.
    pub fn endpoint(&self, https: bool) -> Option<&str> {
        if https {
            self.https.as_deref()
        } else {
            self.http.as_deref()
        }
    }

    /// The endpoint for a scheme with any embedded credential removed.
    pub fn redacted_endpoint(&self, https: bool) -> Option<String> {
        self.endpoint(https).map(redact_endpoint)
    }
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("http", &self.http.as_deref().map(redact_endpoint))
            .field("https", &self.https.as_deref().map(redact_endpoint))
            .field("exceptions", &self.exceptions)
            .finish()
    }
}

/// Which authority configured the proxy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProxySource {
    /// The desktop system settings.
    #[default]
    System,
    /// Explicit proxy environment variables.
    Env,
}

/// A system configuration the client cannot evaluate, reported so a user can
/// be told what to do instead of being given a guessed proxy.
#[derive(Clone, PartialEq, Eq)]
pub enum ProxyDiagnostic {
    /// Automatic proxy configuration with no static endpoint.
    PacUnsupported {
        /// The configured automatic configuration address.
        auto_config_url: String,
    },
}

impl fmt::Display for ProxyDiagnostic {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ProxyDiagnostic::PacUnsupported { auto_config_url } => write!(
                f,
                "the system proxy uses automatic configuration ({}), which this client cannot \
                 evaluate; set HTTPS_PROXY and HTTP_PROXY to the proxy address, or turn the \
                 automatic configuration off",
                redact_endpoint(auto_config_url)
            ),
        }
    }
}

impl fmt::Debug for ProxyDiagnostic {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ProxyDiagnostic::PacUnsupported { auto_config_url } => f
                .debug_struct("PacUnsupported")
                .field("auto_config_url", &redact_endpoint(auto_config_url))
                .finish_non_exhaustive(),
        }
    }
}

/// One resolved proxy state: a configuration, a diagnostic, or neither.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyResolution {
    /// The authority that configured the proxy.
    pub source: ProxySource,
    /// The configuration, when one resolved.
    pub proxy: Option<ProxyConfig>,
    /// The configuration that could not be resolved.
    pub diagnostic: Option<ProxyDiagnostic>,
}

impl ProxyResolution {
    /// No proxy anywhere: consumers connect direct.
    pub fn direct() -> Self {
        Self::default()
    }

    /// A configuration from explicit proxy environment variables.
    pub fn env(proxy: ProxyConfig) -> Self {
        Self {
            source: ProxySource::Env,
            proxy: Some(proxy),
            diagnostic: None,
        }
    }

    /// A configuration from the desktop system settings.
    pub fn system(proxy: ProxyConfig) -> Self {
        Self {
            source: ProxySource::System,
            proxy: Some(proxy),
            diagnostic: None,
        }
    }

    /// A system configuration naming automatic setup without a static endpoint.
    pub fn pac(auto_config_url: String) -> Self {
        Self {
            source: ProxySource::System,
            proxy: None,
            diagnostic: Some(ProxyDiagnostic::PacUnsupported { auto_config_url }),
        }
    }
}

/// A resolved proxy frozen at one moment, tagged with the generation of its
/// resolution.
#[derive(Clone, Debug)]
pub struct ProxySnapshot {
    resolution: ProxyResolution,
    generation: u64,
}

impl ProxySnapshot {
    /// A snapshot that proxies nothing.
    pub fn direct() -> Self {
        Self {
            resolution: ProxyResolution::direct(),
            generation: 0,
        }
    }

    /// The full resolution this snapshot was built from.
    pub fn resolution(&self) -> &ProxyResolution {
        &self.resolution
    }

    /// The configuration, when one resolved.
    pub fn proxy(&self) -> Option<&ProxyConfig> {
        self.resolution.proxy.as_ref()
    }

    /// The authority that configured the proxy.
    pub fn source(&self) -> ProxySource {
        self.resolution.source
    }

    /// The configuration that could not be resolved.
    pub fn diagnostic(&self) -> Option<&ProxyDiagnostic> {
        self.resolution.diagnostic.as_ref()
    }

    /// Rises when the resolution changes, so a consumer knows to rebuild.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// The proxy configuration the environment names, when one does.
pub(crate) fn env_proxy() -> Option<ProxyResolution> {
    let settings = EnvProxySettings {
        https: first_env(&["HTTPS_PROXY", "https_proxy"]),
        http: first_env(&["HTTP_PROXY", "http_proxy"]),
        all: first_env(&["ALL_PROXY", "all_proxy"]),
        no_proxy: first_env(&["NO_PROXY", "no_proxy"]),
    };
    proxy_from_env(&settings)
}

/// The values the environment proxy configuration is parsed from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct EnvProxySettings {
    https: Option<String>,
    http: Option<String>,
    all: Option<String>,
    no_proxy: Option<String>,
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| var(key).ok())
        .find(|value| !value.trim().is_empty())
}

/// A configuration from explicit environment settings. A proxy variable makes
/// the configuration authoritative: a scheme with no endpoint sends that
/// scheme direct, and only a configured scheme that fails is held against the
/// caller.
fn proxy_from_env(settings: &EnvProxySettings) -> Option<ProxyResolution> {
    let https = settings
        .https
        .as_deref()
        .or(settings.all.as_deref())
        .and_then(proxy_url);
    let http = settings
        .http
        .as_deref()
        .or(settings.all.as_deref())
        .and_then(proxy_url);
    if https.is_none() && http.is_none() {
        return None;
    }
    let exceptions = settings
        .no_proxy
        .as_deref()
        .map(expand_bypass_rules)
        .unwrap_or_default();
    Some(ProxyResolution::env(ProxyConfig::new(
        http, https, exceptions,
    )))
}

/// The endpoints a ProxyServer value names. A bare host and port applies to
/// both schemes; a protocol list names each scheme separately, and a protocol
/// this client does not use is ignored.
pub fn parse_proxy_endpoints(raw: &str) -> Option<(Option<String>, Option<String>)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if !raw.contains('=') {
        let url = proxy_url(raw)?;
        return Some((Some(url.clone()), Some(url)));
    }
    let mut http = None;
    let mut https = None;
    for entry in raw.split(';') {
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match name.trim().to_ascii_lowercase().as_str() {
            "http" => http = proxy_url(value),
            "https" => https = proxy_url(value),
            _ => {}
        }
    }
    if http.is_none() && https.is_none() {
        None
    } else {
        Some((http, https))
    }
}

/// A proxy endpoint as an HTTP client URL. A value without a scheme is the
/// host and port form the desktop settings use.
fn proxy_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.contains("://") {
        Some(value.to_string())
    } else {
        Some(format!("http://{value}"))
    }
}

/// Merge an environment bypass variable into a desktop proxy resolution when
/// no environment proxy overrode the desktop settings.
pub(crate) fn merge_env_no_proxy(resolution: ProxyResolution) -> ProxyResolution {
    merge_no_proxy(resolution, first_env(&["NO_PROXY", "no_proxy"]).as_deref())
}

fn merge_no_proxy(mut resolution: ProxyResolution, raw_no_proxy: Option<&str>) -> ProxyResolution {
    let Some(extra) = raw_no_proxy
        .map(expand_bypass_rules)
        .filter(|s| !s.is_empty())
    else {
        return resolution;
    };
    if let Some(config) = &mut resolution.proxy {
        if config.exceptions.is_empty() {
            config.exceptions = extra;
        } else {
            config.exceptions.push(',');
            config.exceptions.push_str(&extra);
        }
    }
    resolution
}

/// Bypass rules in the comma-separated form the HTTP client accepts. Desktop
/// settings write domain suffixes with a leading star-dot and address prefixes
/// with a trailing star, whereas the HTTP client only treats a lone star as a
/// wildcard and matches domain suffixes through a leading dot and subnets
/// through CIDR notation.
pub(crate) fn expand_bypass_rules(raw: &str) -> String {
    raw.split([';', ','])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(normalize_bypass_entry)
        .collect::<Vec<_>>()
        .join(",")
}

fn normalize_bypass_entry(item: &str) -> String {
    if item.eq_ignore_ascii_case("<local>") {
        return "localhost,127.0.0.1,::1".to_string();
    }
    if let Some(suffix) = item.strip_prefix("*.")
        && !suffix.is_empty()
        && !suffix.contains('*')
    {
        return format!(".{suffix}");
    }
    if let Some(cidr) = expand_address_wildcard(item) {
        return cidr;
    }
    item.to_string()
}

fn expand_address_wildcard(item: &str) -> Option<String> {
    let prefix = item.strip_suffix(".*")?;
    let octets: Vec<u8> = prefix
        .split('.')
        .map(|part| part.parse::<u8>().ok())
        .collect::<Option<Vec<_>>>()?;
    match octets.as_slice() {
        [a] => Some(format!("{a}.0.0.0/8")),
        [a, b] => Some(format!("{a}.{b}.0.0/16")),
        [a, b, c] => Some(format!("{a}.{b}.{c}.0/24")),
        _ => None,
    }
}

/// Whether an endpoint names this machine. Only a proxy on this machine gets
/// the bounded fallback the web fetch tool allows.
pub fn is_loopback_endpoint(endpoint: &str) -> bool {
    let host = endpoint_host(endpoint);
    host == "localhost"
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn endpoint_host(endpoint: &str) -> &str {
    let after_scheme = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(after_scheme);
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
}

/// An endpoint with any embedded credential removed. The user and password
/// before the host are stripped so a proxy URL never carries a secret into a
/// diagnostic, a warning, or a log line.
pub(crate) fn redact_endpoint(endpoint: &str) -> String {
    let (scheme, rest) = match endpoint.split_once("://") {
        Some((scheme, rest)) => (Some(scheme), rest),
        None => (None, endpoint),
    };
    let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    match scheme {
        Some(scheme) => format!("{scheme}://{host}{tail}"),
        None => format!("{host}{tail}"),
    }
}

#[cfg(test)]
mod tests;
