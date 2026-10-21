//! Resolve a host once, and connect only to the addresses that passed the
//! private-address screen.
//!
//! The fetch path used to resolve a host in order to screen it and then let
//! the HTTP client resolve it again in order to connect. Those are two answers
//! to the same question, and a name whose second answer points somewhere
//! private slips through the screen. This resolver is the connection's own
//! lookup, so the addresses that were screened are the addresses a socket is
//! opened to, and there is no second answer.

use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use reqwest::dns::Addrs;
use reqwest::dns::Name;
use reqwest::dns::Resolve;
use reqwest::dns::Resolving;
use tokio::net::lookup_host;

use super::target::is_private_ip;

/// Prefix of the error a refused lookup reports. The transport classifier
/// reads it so the model sees a private-target failure rather than a generic
/// connect error.
pub(crate) const PRIVATE_TARGET: &str = "webfetch: target resolves to a private address";

/// A resolver that refuses a host whose answer is not public.
pub(crate) struct PublicResolver {
    /// A fixed answer, so a test can hand back a loopback address without
    /// depending on a real name.
    fixed: Option<Vec<SocketAddr>>,
    /// How many lookups this resolver performed. The test that proves a
    /// request resolves its host once reads this.
    lookups: Arc<AtomicUsize>,
}

impl PublicResolver {
    /// Resolve names through the system resolver, screening every answer.
    pub(crate) fn new() -> Self {
        Self {
            fixed: None,
            lookups: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Answer every name with a fixed set of addresses. Test-only.
    #[cfg(test)]
    pub(crate) fn with_fixed(addrs: Vec<SocketAddr>) -> Self {
        Self {
            fixed: Some(addrs),
            lookups: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The number of lookups performed so far.
    #[cfg(test)]
    pub(crate) fn lookup_count(&self) -> usize {
        self.lookups.load(Ordering::Relaxed)
    }
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        let host = name.as_str().to_string();
        let fixed = self.fixed.clone();
        Box::pin(async move {
            let addresses: Vec<SocketAddr> = match fixed {
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
