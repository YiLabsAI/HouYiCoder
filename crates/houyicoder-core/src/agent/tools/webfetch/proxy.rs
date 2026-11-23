//! The web fetch transport: one client rebuilt per proxy generation, with a
//! direct fallback that fires only when a local system proxy refused.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use houyicoder_platform::host::net::{
    ProxyConfig, ProxyConfigCache, ProxySnapshot, ProxySource, is_loopback_endpoint,
};
use reqwest::redirect::Policy;
use reqwest::{Client, NoProxy, Proxy};

use super::CONNECT_TIMEOUT_SECS;
use super::FETCH_TIMEOUT_SECS;
use super::TCP_KEEPALIVE_SECS;
use super::redirect::{FetchError, fetch_following};
use super::resolver::PublicResolver;
use super::response::proxy_config_payload;

/// How long a local system proxy stays skipped after it refused a connection.
pub(crate) const FALLBACK_WINDOW: Duration = Duration::from_secs(60);

/// A fetch client that sends through a proxy, tagged with whether a refused
/// connection may fall back to direct.
#[derive(Clone, Debug)]
pub(crate) struct ProxyClient {
    pub(crate) client: Client,
    source: ProxySource,
    loopback: bool,
}

impl ProxyClient {
    /// Build a proxy client from its endpoint client, the authority that
    /// configured it, and whether the endpoint is on this machine.
    #[cfg(test)]
    pub(crate) fn new(client: Client, source: ProxySource, loopback: bool) -> Self {
        Self {
            client,
            source,
            loopback,
        }
    }

    /// Only a system proxy whose endpoint is on this machine may fall back
    /// after a refusal; an explicit environment proxy never does.
    pub(crate) fn may_fall_back(&self) -> bool {
        self.source == ProxySource::System && self.loopback
    }
}

/// The single web fetch transport. One lock guards the cached proxy client and
/// the bounded fallback window. The direct client is built on first use and
/// serves both the no-proxy path and the fallback path.
pub(crate) struct FetchClient {
    direct: OnceLock<Client>,
    resolver: Arc<PublicResolver>,
    cache: Arc<ProxyConfigCache>,
    state: Mutex<FetchState>,
}

/// The transport state for one proxy generation: the built client, the
/// diagnostic when it could not be built, and the fallback window a refused
/// local proxy opened.
#[derive(Default)]
struct FetchState {
    generation: Option<u64>,
    proxy: Option<ProxyClient>,
    diagnostic: Option<String>,
    fallback_until: Option<Instant>,
}

impl FetchClient {
    /// Whether the direct transport has been created. A test asserts that
    /// assembling a tool leaves it unbuilt, so a later eager build is caught.
    #[cfg(test)]
    pub(crate) fn transport_built(&self) -> bool {
        self.direct.get().is_some()
    }

    /// The direct transport, created on the first request. Loading the
    /// platform trust store is the expensive part of building one, and a
    /// session that never fetches a page must not pay it at assembly. The
    /// first request therefore carries that load on the runtime thread that
    /// makes it; every later request reads the initialized transport.
    fn direct_client(&self) -> &Client {
        self.direct
            .get_or_init(|| direct_client(Arc::clone(&self.resolver)))
    }

    pub(crate) fn new(resolver: Arc<PublicResolver>, cache: Arc<ProxyConfigCache>) -> Self {
        Self {
            direct: OnceLock::new(),
            resolver,
            cache,
            state: Mutex::new(FetchState::default()),
        }
    }

    /// Fetch a target, returning the response and any fallback warning.
    pub(crate) async fn send(
        &self,
        target: &str,
    ) -> Result<(reqwest::Response, Option<String>), FetchError> {
        let snapshot = self.cache.snapshot();
        let (proxy, in_cooldown, diagnostic) = self.refresh(&snapshot);
        if let Some(message) = diagnostic {
            return Err(FetchError::Refused(proxy_config_payload(&message)));
        }
        let active = if in_cooldown { None } else { proxy.as_ref() };
        let mut fell_back = false;
        let response = fetch_following(
            self.direct_client(),
            active,
            Some(&self.resolver),
            target,
            &mut fell_back,
        )
        .await?;
        let warning = if fell_back {
            self.note_fallback();
            Some(fallback_warning())
        } else if in_cooldown {
            Some(cooldown_warning())
        } else {
            None
        };
        Ok((response, warning))
    }

    fn refresh(&self, snapshot: &ProxySnapshot) -> (Option<ProxyClient>, bool, Option<String>) {
        let mut state = lock(&self.state);
        if state.generation != Some(snapshot.generation()) {
            state.generation = Some(snapshot.generation());
            state.fallback_until = None;
            match build_proxy_client(snapshot, Arc::clone(&self.resolver)) {
                Ok(client) => {
                    state.proxy = client;
                    state.diagnostic = None;
                }
                Err(message) => {
                    state.proxy = None;
                    state.diagnostic = Some(message);
                }
            }
        }
        let now = Instant::now();
        let in_cooldown = state.fallback_until.is_some_and(|at| now < at);
        (state.proxy.clone(), in_cooldown, state.diagnostic.clone())
    }

    fn note_fallback(&self) {
        let mut state = lock(&self.state);
        state.fallback_until = Some(Instant::now() + FALLBACK_WINDOW);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The builder every direct fetch client starts from. The system proxy is
/// turned off on this client so it serves as the clean fallback whenever no
/// proxy is configured or a local proxy refused the connection.
fn direct_client(resolver: Arc<PublicResolver>) -> Client {
    Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS))
        .no_proxy()
        .dns_resolver(resolver)
        .build()
        .expect("HTTP client construction failed (TLS backend error)")
}

fn build_proxy_client(
    snapshot: &ProxySnapshot,
    resolver: Arc<PublicResolver>,
) -> Result<Option<ProxyClient>, String> {
    if let Some(diagnostic) = snapshot.diagnostic() {
        return Err(diagnostic.to_string());
    }
    let Some(proxy) = snapshot.proxy() else {
        return Ok(None);
    };
    let client = client_from_config(proxy, resolver)?;
    let loopback = loopback_only(proxy);
    Ok(Some(ProxyClient {
        client,
        source: snapshot.source(),
        loopback,
    }))
}

/// Build a client that sends each configured scheme through its endpoint. A
/// scheme with no endpoint sends direct by the standard semantics, so a user
/// who sets only the TLS proxy still reaches a local plain model endpoint.
/// Whether every configured endpoint is on this machine. Split from the
/// client so the rule is stated without building a transport.
fn loopback_only(proxy: &ProxyConfig) -> bool {
    proxy
        .http
        .iter()
        .chain(proxy.https.iter())
        .all(|endpoint| is_loopback_endpoint(endpoint))
}

fn client_from_config(
    proxy: &ProxyConfig,
    resolver: Arc<PublicResolver>,
) -> Result<Client, String> {
    let mut builder = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS))
        .dns_resolver(resolver);
    let bypass = (!proxy.exceptions.is_empty())
        .then(|| NoProxy::from_string(&proxy.exceptions))
        .flatten();
    for (is_https, endpoint) in [(true, &proxy.https), (false, &proxy.http)] {
        let Some(endpoint) = endpoint else {
            continue;
        };
        let entry = if is_https {
            Proxy::https(endpoint)
        } else {
            Proxy::http(endpoint)
        }
        .map_err(|error| format!("the proxy endpoint is not usable: {error}"))?;
        builder = builder.proxy(entry.no_proxy(bypass.clone()));
    }
    builder
        .build()
        .map_err(|error| format!("the proxy client could not be built: {error}"))
}

fn fallback_warning() -> String {
    format!(
        "the system proxy refused the connection, so this fetch used a direct connection; the \
         proxy is skipped for {} seconds",
        FALLBACK_WINDOW.as_secs()
    )
}

fn cooldown_warning() -> String {
    format!(
        "the system proxy refused a recent connection; this fetch used a direct connection and \
         the proxy is retried after {} seconds",
        FALLBACK_WINDOW.as_secs()
    )
}

#[cfg(test)]
mod tests;
