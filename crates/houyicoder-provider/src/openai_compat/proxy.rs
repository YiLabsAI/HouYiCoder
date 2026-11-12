//! The provider HTTP transport: one client rebuilt per proxy generation. A
//! configuration that cannot be used, or a proxy that cannot carry the
//! request, fails it rather than falling back to a direct connection.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use houyicoder_platform::host::net::{ProxyConfigCache, ProxySnapshot};
use houyicoder_protocol::llm::ProviderError;
use reqwest::{Client, NoProxy, Proxy};

/// The single provider HTTP transport. One lock guards the cached client built
/// for the current proxy generation. The provider never falls back to a direct
/// connection when a configured proxy cannot be used.
pub(crate) struct HttpTransport {
    cache: Arc<ProxyConfigCache>,
    state: Mutex<HttpState>,
}

/// The transport state for one proxy generation: the built client, or the
/// diagnostic when it could not be built.
#[derive(Default)]
struct HttpState {
    generation: Option<u64>,
    client: Option<Client>,
    diagnostic: Option<String>,
}

impl HttpTransport {
    pub(crate) fn new(cache: Arc<ProxyConfigCache>) -> Self {
        Self {
            cache,
            state: Mutex::new(HttpState::default()),
        }
    }

    /// The client for the current configuration generation. A proxy that
    /// cannot be used fails the request; it never falls back to a direct
    /// connection.
    pub(crate) fn client(&self) -> Result<Client, ProviderError> {
        let snapshot = self.cache.snapshot();
        self.refresh(&snapshot)
    }

    fn refresh(&self, snapshot: &ProxySnapshot) -> Result<Client, ProviderError> {
        let mut state = lock(&self.state);
        if state.generation != Some(snapshot.generation()) {
            state.generation = Some(snapshot.generation());
            match build_client(snapshot) {
                Ok(client) => {
                    state.client = Some(client);
                    state.diagnostic = None;
                }
                Err(message) => {
                    state.client = None;
                    state.diagnostic = Some(message);
                }
            }
        }
        if let Some(message) = &state.diagnostic {
            return Err(ProviderError::Unknown(message.clone()));
        }
        state
            .client
            .clone()
            .ok_or_else(|| ProviderError::Unknown("no HTTP client is available".into()))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Build the provider client for one proxy snapshot. A scheme with no endpoint
/// sends direct by the standard semantics, so a user who sets only the TLS
/// proxy still reaches a local plain model endpoint. A configuration that
/// cannot be evaluated fails here instead of connecting direct.
fn build_client(snapshot: &ProxySnapshot) -> Result<Client, String> {
    if let Some(diagnostic) = snapshot.diagnostic() {
        return Err(diagnostic.to_string());
    }
    let mut builder = Client::builder()
        .read_timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30));
    let Some(proxy) = snapshot.proxy() else {
        return builder
            .no_proxy()
            .build()
            .map_err(|error| format!("the HTTP client could not be built: {error}"));
    };
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
        .map_err(|error| format!("the HTTP client could not be built: {error}"))
}
