//! The proxy cache: one snapshot per process, refreshed off the request
//! thread.
//!
//! A snapshot inside its lifetime is returned as is. Past it, the snapshot
//! already held is returned and the settings are read on a background thread,
//! so a request thread never waits for the desktop read. A configuration the
//! user changes takes effect on the request after that read lands, which is
//! one call later than reading on every caller would give.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::Builder;
use std::time::{Duration, Instant};

use crate::host::system_proxy;

use super::{PROXY_CONFIG_TTL, ProxyResolution, ProxySnapshot};

/// Reads the current proxy configuration from the environment or the system
/// settings.
pub type ProxyQuery = Arc<dyn Fn() -> ProxyResolution + Send + Sync>;

/// How long a background read may hold the refresh slot before a later caller
/// treats it as stuck and starts another. The stale snapshot keeps being
/// returned meanwhile, so a settings call that never answers costs freshness,
/// not availability.
pub(super) const REFRESH_BOUND: Duration = Duration::from_secs(60);

struct CacheState {
    fetched_at: Option<Instant>,
    snapshot: Arc<ProxySnapshot>,
    /// When the running background read started, while one holds the slot.
    refresh_started: Option<Instant>,
    /// Identifies the read that owns the refresh slot. A read that began
    /// before a newer one may return after it, so only the read holding the
    /// current epoch applies its result or clears the slot.
    refresh_epoch: u64,
}

impl CacheState {
    fn is_fresh(&self, now: Instant, ttl: Duration) -> bool {
        self.fetched_at
            .is_some_and(|at| now.saturating_duration_since(at) < ttl)
    }

    /// Whether a background read holds the slot and is still within its
    /// bound.
    fn refresh_running(&self, now: Instant) -> bool {
        self.refresh_started
            .is_some_and(|at| now.saturating_duration_since(at) < REFRESH_BOUND)
    }
}

/// Releases the refresh slot when dropped, so a read that panics does not
/// leave the cache unable to refresh again. A read that no longer owns the
/// slot leaves it to the read that does.
struct RefreshGuard {
    state: Arc<Mutex<CacheState>>,
    epoch: u64,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        let mut state = lock_state(&self.state);
        if state.refresh_epoch == self.epoch {
            state.refresh_started = None;
        }
    }
}

/// A proxy configuration cache with a fixed lifetime.
///
/// A snapshot inside its lifetime is returned as is. Past it, the snapshot
/// already held is returned and the settings are read on a background thread,
/// so a request thread never waits for the desktop read; the request built
/// from the old snapshot is already under way, and the refresh lands before
/// the next one. One background read holds the slot at a time, so a burst of
/// requests at the expiry instant starts one read rather than one per
/// caller, and a read still running past its bound gives the slot up so a
/// settings call that never answers cannot freeze the cache. A read that
/// began before a newer one and returns after it does not replace the newer
/// snapshot.
///
/// The first read, with no snapshot to serve, runs on the calling thread: a
/// guessed direct snapshot could send a proxied request direct for as long as
/// the read takes. Two callers that find no snapshot both read, because
/// there is no answer to hand one of them yet. A lifetime of zero disables
/// reuse, so every call reads on the calling thread.
pub struct ProxyConfigCache {
    ttl: Duration,
    query: ProxyQuery,
    state: Arc<Mutex<CacheState>>,
}

impl ProxyConfigCache {
    /// A cache over the given query and lifetime.
    pub fn with_query(ttl: Duration, query: ProxyQuery) -> Arc<Self> {
        Arc::new(Self {
            ttl,
            query,
            state: Arc::new(Mutex::new(CacheState {
                fetched_at: None,
                snapshot: Arc::new(ProxySnapshot::direct()),
                refresh_started: None,
                refresh_epoch: 0,
            })),
        })
    }

    /// A cache that always answers with no proxy, for a consumer that must
    /// never consult the system settings and for tests. The answer cannot
    /// change, so nothing is ever refreshed.
    pub fn direct() -> Arc<Self> {
        Self::with_query(Duration::MAX, Arc::new(ProxyResolution::direct))
    }

    /// The process-wide cache shared by every consumer.
    pub fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<ProxyConfigCache>> = OnceLock::new();
        Arc::clone(
            GLOBAL.get_or_init(|| Self::with_query(PROXY_CONFIG_TTL, Arc::new(system_proxy))),
        )
    }

    /// The current snapshot, refreshing it when the lifetime has elapsed.
    pub fn snapshot(&self) -> Arc<ProxySnapshot> {
        self.snapshot_at(Instant::now())
    }

    /// The snapshot as of a given moment, so the lifetime is testable without
    /// sleeping.
    pub fn snapshot_at(&self, now: Instant) -> Arc<ProxySnapshot> {
        // A lifetime of zero is a caller asking for no reuse, so a snapshot
        // kept from an earlier call is exactly what it ruled out.
        if self.ttl.is_zero() {
            return self.refresh_now(now);
        }
        let mut state = lock_state(&self.state);
        if state.is_fresh(now, self.ttl) {
            return Arc::clone(&state.snapshot);
        }
        if state.fetched_at.is_none() {
            drop(state);
            return self.refresh_now(now);
        }
        let stale = Arc::clone(&state.snapshot);
        let begin = !state.refresh_running(now);
        let epoch = if begin {
            state.refresh_epoch = state.refresh_epoch.wrapping_add(1);
            state.refresh_started = Some(now);
            state.refresh_epoch
        } else {
            state.refresh_epoch
        };
        drop(state);
        if begin && !self.start_background_refresh(epoch) {
            // A thread that cannot start is the one case where the read
            // falls back to this thread: the stale snapshot then does not
            // outlive its lifetime by more than one call.
            tracing::warn!("proxy refresh thread did not start; reading on the calling thread");
            return self.refresh_now(now);
        }
        stale
    }

    /// Read the settings on this thread and store the result.
    ///
    /// A read on the calling thread answers the caller that asked for it, so
    /// its result always applies. It runs only while no background read owns
    /// the slot: on a cold start there is nothing to refresh yet, a zero
    /// lifetime never starts one, and a thread that failed to start left none
    /// running.
    fn refresh_now(&self, now: Instant) -> Arc<ProxySnapshot> {
        let epoch = lock_state(&self.state).refresh_epoch;
        let _guard = RefreshGuard {
            state: Arc::clone(&self.state),
            epoch,
        };
        let resolution = (self.query)();
        store_snapshot(&self.state, now, resolution, None)
    }

    /// Start one background read, answering whether the thread started.
    fn start_background_refresh(&self, epoch: u64) -> bool {
        let state = Arc::clone(&self.state);
        let query = Arc::clone(&self.query);
        Builder::new()
            .name("proxy-config-refresh".to_string())
            .spawn(move || {
                let _guard = RefreshGuard {
                    state: Arc::clone(&state),
                    epoch,
                };
                let resolution = query();
                store_snapshot(&state, Instant::now(), resolution, Some(epoch));
            })
            .is_ok()
    }
}

fn lock_state(state: &Mutex<CacheState>) -> MutexGuard<'_, CacheState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Store a resolution as the cache's snapshot, raising the generation when the
/// resolution changed, and release the refresh slot. A background read that
/// no longer owns the slot is discarded, because a read that started after it
/// has already answered.
fn store_snapshot(
    state: &Mutex<CacheState>,
    fetched_at: Instant,
    resolution: ProxyResolution,
    owner: Option<u64>,
) -> Arc<ProxySnapshot> {
    let mut state = lock_state(state);
    if owner.is_some_and(|epoch| epoch != state.refresh_epoch) {
        return Arc::clone(&state.snapshot);
    }
    let generation = if state.snapshot.resolution == resolution {
        state.snapshot.generation
    } else {
        state.snapshot.generation.saturating_add(1)
    };
    let snapshot = Arc::new(ProxySnapshot {
        resolution,
        generation,
    });
    state.snapshot = Arc::clone(&snapshot);
    state.fetched_at = Some(fetched_at);
    state.refresh_started = None;
    snapshot
}
