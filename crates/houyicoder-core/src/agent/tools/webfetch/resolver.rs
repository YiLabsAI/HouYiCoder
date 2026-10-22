//! Resolve a host once per hop, screen every answer against private ranges,
//! and serve the screened addresses to the HTTP connector without a second OS
//! lookup.
//!
//! When a proxy is active, the HTTP client sends the hostname to the proxy
//! instead of calling its own DNS resolver. Running screen_target before each
//! hop guarantees that private and loopback answers are refused on both the
//! proxy path and the direct path, while caching the validated addresses in
//! the resolver ensures the direct connector reuses the exact same lookup.

use std::collections::HashMap;
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use reqwest::Url;
use reqwest::dns::Addrs;
use reqwest::dns::Name;
use reqwest::dns::Resolve;
use reqwest::dns::Resolving;
use serde_json::Value;
use serde_json::json;
use tokio::net::lookup_host;

use super::redirect::is_private_ip;

/// Prefix of the error a refused lookup reports.
pub(crate) const PRIVATE_TARGET: &str = "webfetch: target resolves to a private address";

/// A resolver that screens every host before a hop is sent and hands the
/// validated addresses to the direct connector without a second OS lookup.
pub(crate) struct PublicResolver {
    /// A preset sequence of answers for tests. Each lookup pops the first
    /// remaining entry, or clones the last one when only one remains, so a
    /// rebinding test can supply a public address first and a loopback
    /// address second.
    answers: Option<Mutex<Vec<Vec<SocketAddr>>>>,
    /// Validated addresses from screen_target, keyed by lowercase host. The
    /// direct connector takes from here so each hop performs at most one OS
    /// lookup.
    validated: Mutex<HashMap<String, Vec<SocketAddr>>>,
    /// How many underlying lookups this resolver performed.
    lookups: Arc<AtomicUsize>,
}

impl PublicResolver {
    /// Resolve names through the system resolver, screening every answer.
    pub(crate) fn new() -> Self {
        Self {
            answers: None,
            validated: Mutex::new(HashMap::new()),
            lookups: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Answer lookups with a fixed address list. Test-only.
    #[cfg(test)]
    pub(crate) fn with_fixed(addrs: Vec<SocketAddr>) -> Self {
        Self::with_sequence(vec![addrs])
    }

    /// Answer consecutive lookups with preset address lists. Test-only.
    #[cfg(test)]
    pub(crate) fn with_sequence(sequence: Vec<Vec<SocketAddr>>) -> Self {
        Self {
            answers: Some(Mutex::new(sequence)),
            validated: Mutex::new(HashMap::new()),
            lookups: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The number of underlying lookups performed so far.
    #[cfg(test)]
    pub(crate) fn lookup_count(&self) -> usize {
        self.lookups.load(Ordering::Relaxed)
    }

    /// Resolve the URL's host once, refuse any private or loopback address,
    /// and store the validated addresses for the next direct connection to
    /// consume.
    pub(crate) async fn screen_target(&self, url: &str) -> Result<(), Value> {
        let Ok(parsed) = Url::parse(url) else {
            return Ok(());
        };
        let Some(host) = parsed.host_str() else {
            return Ok(());
        };
        let port = parsed.port_or_known_default().unwrap_or(443);
        let Ok(addresses) = self.lookup_raw(host, port).await else {
            return Ok(());
        };
        for address in &addresses {
            if is_private_ip(address.ip()) {
                return Err(json!({
                    "error": format!("{PRIVATE_TARGET} ({})", address.ip()),
                    "error_kind": "private_target",
                }));
            }
        }
        if !addresses.is_empty()
            && let Ok(mut map) = self.validated.lock()
        {
            map.insert(host.to_ascii_lowercase(), addresses);
        }
        Ok(())
    }

    async fn lookup_raw(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        if let Some(ref seq) = self.answers {
            let mut guard = seq.lock().expect("answers mutex");
            let addrs = if guard.len() > 1 {
                guard.remove(0)
            } else {
                guard.first().cloned().unwrap_or_default()
            };
            return Ok(addrs);
        }
        lookup_host((host, port))
            .await
            .map(|iter| iter.collect())
            .map_err(|e| e.to_string())
    }
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_ascii_lowercase();
        let cached = self
            .validated
            .lock()
            .ok()
            .and_then(|mut map| map.remove(&host));
        if let Some(addresses) = cached {
            return Box::pin(async move { Ok(Box::new(addresses.into_iter()) as Addrs) });
        }
        let preset = self.answers.as_ref().map(|seq| {
            let mut guard = seq.lock().expect("answers mutex");
            if guard.len() > 1 {
                guard.remove(0)
            } else {
                guard.first().cloned().unwrap_or_default()
            }
        });
        self.lookups.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            let addresses: Vec<SocketAddr> = match preset {
                Some(addrs) => addrs,
                None => lookup_host((host.as_str(), 0))
                    .await
                    .map_err(|e| -> Box<dyn Error + Send + Sync> { Box::new(e) })?
                    .collect(),
            };
            if addresses.is_empty() {
                return Err(format!("webfetch: no address for {host}").into());
            }
            if let Some(private) = addresses.iter().find(|addr| is_private_ip(addr.ip())) {
                return Err(format!("{PRIVATE_TARGET} ({})", private.ip()).into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests;
