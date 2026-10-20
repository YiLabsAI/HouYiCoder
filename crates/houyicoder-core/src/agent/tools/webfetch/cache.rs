//! The fetch tool's short-lived reuse state: pages it can answer from and
//! connection failures it remembers, both bounded.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long a fetched page stays reusable for the same URL.
pub(crate) const PAGE_TTL: Duration = Duration::from_secs(15 * 60);

/// How long a URL that just failed to connect stays answered from that
/// failure. Short on purpose: a transient network fault clears, and the tool
/// must not keep a host unreachable after it recovers.
pub(crate) const FAILURE_TTL: Duration = Duration::from_secs(60);

/// Upper bound on cached pages, so a long session cannot grow this cache
/// without limit. The oldest page is dropped when the bound is reached.
pub(crate) const CACHE_MAX_ENTRIES: usize = 32;

/// Upper bound on remembered connection failures, for the same reason: a
/// session that probes many distinct dead URLs must not grow this map without
/// limit. The oldest failure is dropped when the bound is reached.
pub(crate) const FAILURE_MAX_ENTRIES: usize = 32;

/// A page fetched once, kept for a short reuse window.
pub(crate) struct CachedPage {
    content: String,
    content_type: String,
    fetched: Instant,
}

/// A URL whose connection attempt failed, kept only long enough that a blind
/// retry in the same turn does not pay the timeout again.
pub(crate) struct FailedUrl {
    pub(crate) kind: &'static str,
    pub(crate) message: String,
    pub(crate) failed: Instant,
}

/// Per-tool reuse state: successful pages and recent connection failures,
/// both keyed by the URL as requested (before the HTTPS upgrade), so the key
/// matches what a caller asks for.
#[derive(Default)]
pub(crate) struct FetchCache {
    pub(crate) pages: HashMap<String, CachedPage>,
    pub(crate) failures: HashMap<String, FailedUrl>,
}

impl FetchCache {
    /// A page still inside its reuse window.
    pub(crate) fn page(&mut self, url: &str) -> Option<(String, String)> {
        self.page_at(url, Instant::now())
    }

    /// A page still inside its reuse window at a stated instant. The instant
    /// is a parameter so a test can state an expired entry instead of waiting
    /// out the window.
    pub(crate) fn page_at(&mut self, url: &str, now: Instant) -> Option<(String, String)> {
        let page = self.pages.get(url)?;
        if now.saturating_duration_since(page.fetched) >= PAGE_TTL {
            self.pages.remove(url);
            return None;
        }
        Some((page.content.clone(), page.content_type.clone()))
    }

    /// Keep a page for reuse, dropping the oldest entry when the cache is
    /// full. Re-storing a URL already held replaces it in place, so a refresh
    /// does not evict an unrelated page.
    pub(crate) fn store_page(&mut self, url: &str, content: String, content_type: String) {
        if !self.pages.contains_key(url)
            && self.pages.len() >= CACHE_MAX_ENTRIES
            && let Some(oldest) = self
                .pages
                .iter()
                .min_by_key(|(_, page)| page.fetched)
                .map(|(key, _)| key.clone())
        {
            self.pages.remove(&oldest);
        }
        self.pages.insert(
            url.to_string(),
            CachedPage {
                content,
                content_type,
                fetched: Instant::now(),
            },
        );
    }

    /// A connection failure still inside its short window.
    pub(crate) fn failure(&mut self, url: &str) -> Option<(&'static str, String)> {
        self.failure_at(url, Instant::now())
    }

    /// A connection failure still inside its short window at a stated instant,
    /// for the same reason the page check takes one.
    pub(crate) fn failure_at(&mut self, url: &str, now: Instant) -> Option<(&'static str, String)> {
        let failure = self.failures.get(url)?;
        if now.saturating_duration_since(failure.failed) >= FAILURE_TTL {
            self.failures.remove(url);
            return None;
        }
        Some((failure.kind, failure.message.clone()))
    }

    /// Remember that a URL could not be reached, dropping the oldest entry
    /// when the cache is full. Re-storing a URL already held replaces it in
    /// place, so a repeated failure does not evict an unrelated entry.
    pub(crate) fn store_failure(&mut self, url: &str, kind: &'static str, message: String) {
        if !self.failures.contains_key(url)
            && self.failures.len() >= FAILURE_MAX_ENTRIES
            && let Some(oldest) = self
                .failures
                .iter()
                .min_by_key(|(_, failure)| failure.failed)
                .map(|(key, _)| key.clone())
        {
            self.failures.remove(&oldest);
        }
        self.failures.insert(
            url.to_string(),
            FailedUrl {
                kind,
                message,
                failed: Instant::now(),
            },
        );
    }
}
