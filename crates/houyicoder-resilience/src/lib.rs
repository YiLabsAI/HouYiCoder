//! Failure-isolation primitives for the engine.
//!
//! Two concerns live here:
//!
//! retry_with is the cross-cutting retry used by LLM calls, sidecar IPC, and
//! tool dispatch: exponential backoff with jitter and a max-attempt ceiling.
//! Borrows the retry shape and the CLI circuit-breaker instincts, without
//! coupling to any specific call site.
//!
//! CircuitBreaker is a three-state finite machine (Closed / Open / HalfOpen)
//! tracking consecutive failures. Once a configurable threshold is reached it
//! trips Open and rejects calls for a cool-down period, after which a single
//! HalfOpen probe is permitted; success closes, failure re-opens. Retry can
//! consult a breaker via run_guarded so dead-loop-prone call sites (auto
//! compact, sidecar IPC) stop burning API budget instead of retrying forever.
//!
//! resource_breaker is the aggregate breaker for spawned processes: trips on
//! total spawned CPU, in-flight proc count, or consecutive per-cmd budget
//! exceed. A foundation leaf with no internal deps.

use std::error::Error;
use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Formatter;
use std::future::Future;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(test)]
use std::thread::spawn;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Retry configuration: exponential backoff with jitter, capped attempts.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    /// Maximum number of attempts (including the first). Must be >= 1.
    pub max_attempts: u32,
    /// Base delay before the second attempt; doubled each retry.
    pub base_delay: Duration,
    /// Upper bound on a single delay (caps exponential growth).
    pub max_delay: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(8),
        }
    }
}

/// The outcome of a retry loop that exhausted its attempts.
#[derive(Debug)]
pub enum RetryError<E> {
    /// Every attempt failed; the last error is preserved for diagnostics.
    Exhausted { attempts: u32, last: E },
    /// The operation failed with a non-retryable error on the first attempt;
    /// the loop did not retry. Distinct from Exhausted so callers can tell a
    /// single fatal failure from a budget of transient ones.
    Fatal { error: E },
}

impl<E: Display> Display for RetryError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exhausted { attempts, last } => {
                write!(
                    f,
                    "retry exhausted after {attempts} attempts; last error: {last}"
                )
            }
            Self::Fatal { error } => {
                write!(f, "non-retryable error: {error}")
            }
        }
    }
}

impl<E: Debug + Display> Error for RetryError<E> {}

impl Retry {
    /// The delay to wait before the next attempt. Exponential backoff from
    /// base_delay, doubled per attempt, capped at max_delay, with up to 50%
    /// jitter so concurrent retriers desynchronize. A server-suggested delay
    /// (the Retry-After header) bypasses the backoff ceiling — a rate-limited
    /// account is polled after the server's window, not every max-delay tick.
    pub fn delay_for(&self, attempt: u32, server_delay: Option<Duration>) -> Duration {
        if let Some(d) = server_delay {
            return d;
        }
        let exp = attempt.saturating_sub(1);
        let grown = self.base_delay.saturating_mul(2u32.saturating_pow(exp));
        jittered(grown.min(self.max_delay))
    }

    /// Run an operation with exponential backoff until it succeeds or
    /// max_attempts is exhausted. The closure receives the 1-indexed attempt
    /// number. Delays double each retry up to max_delay, with up to 50% jitter
    /// so concurrent retriers desynchronize.
    pub async fn run<F, Fut, T, E>(&self, mut operation: F) -> Result<T, RetryError<E>>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Debug,
    {
        let mut delay = self.base_delay;
        let mut last_err: Option<E> = None;
        for attempt in 1..=self.max_attempts {
            match operation(attempt).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = Some(e);
                    if attempt == self.max_attempts {
                        break;
                    }
                    sleep(jittered(delay)).await;
                    delay = (delay * 2).min(self.max_delay);
                }
            }
        }
        Err(RetryError::Exhausted {
            attempts: self.max_attempts,
            last: last_err.expect("at least one attempt ran"),
        })
    }

    /// Run an operation with exponential backoff, retrying only errors the
    /// is_retryable predicate accepts. Non-retryable errors short-circuit as
    /// RetryError::Fatal on the first attempt (no delay). This is the variant
    /// the LLM call uses: ProviderError::retryable() decides, so Auth or
    /// InvalidRequest don't burn the retry budget.
    pub async fn run_if<F, Fut, T, E, P>(
        &self,
        mut operation: F,
        is_retryable: P,
    ) -> Result<T, RetryError<E>>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Debug,
        P: Fn(&E) -> bool,
    {
        let mut delay = self.base_delay;
        let mut last_err: Option<E> = None;
        for attempt in 1..=self.max_attempts {
            match operation(attempt).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !is_retryable(&e) {
                        return Err(RetryError::Fatal { error: e });
                    }
                    last_err = Some(e);
                    if attempt == self.max_attempts {
                        break;
                    }
                    sleep(jittered(delay)).await;
                    delay = (delay * 2).min(self.max_delay);
                }
            }
        }
        Err(RetryError::Exhausted {
            attempts: self.max_attempts,
            last: last_err.expect("at least one attempt ran"),
        })
    }
}

/// Apply up to ±50% jitter to a delay so concurrent retriers desynchronize
/// (thundering-herd avoidance). Entropy comes from the system clock's
/// subsecond nanos — no rand dependency, and the loop already runs on tokio
/// so a clock read is cheap. Returns a delay in [delay/2, delay*3/2).
fn jittered(delay: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let scale = 0.5 + (nanos as f64) / (u32::MAX as f64); // [0.5, 1.5)
    let ms = delay.as_millis() as f64;
    Duration::from_millis((ms * scale) as u64)
}

/// The finite state of a circuit breaker guarding an operation against
/// cascading failures. Closed lets calls through; Open rejects them; HalfOpen
/// permits a single probe after the cool-down elapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// Tunable knobs for a circuit breaker. Defaults match common industry
/// practice; the threshold is configurable so callers can tune per call site.
#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    /// Consecutive failures required to trip the breaker into Open.
    pub failure_threshold: u32,
    /// How long Open rejects calls before transitioning to HalfOpen.
    pub cool_down: Duration,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cool_down: Duration::from_secs(30),
        }
    }
}

/// Pure-logic state machine. Takes now as a parameter so it is fully
/// testable without time mocks or unsafe global mutation. The wrapping
/// CircuitBreaker injects Instant::now at runtime.
struct BreakerInner {
    state: BreakerState,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    config: BreakerConfig,
}

impl BreakerInner {
    fn new(config: BreakerConfig) -> Self {
        Self {
            state: BreakerState::Closed,
            consecutive_failures: 0,
            opened_at: None,
            config,
        }
    }

    /// Whether a call is permitted right now. Transitions Open to HalfOpen
    /// when the cool-down has elapsed (the probe is the call that follows).
    fn allow(&mut self, now: Instant) -> bool {
        match self.state {
            BreakerState::Closed => true,
            BreakerState::Open => {
                let opened = match self.opened_at {
                    Some(t) => t,
                    None => return true,
                };
                if now.duration_since(opened) >= self.config.cool_down {
                    self.state = BreakerState::HalfOpen;
                    true
                } else {
                    false
                }
            }
            BreakerState::HalfOpen => true,
        }
    }

    /// Record a failed call. In Closed, accumulates until the threshold trips
    /// Open. In HalfOpen, immediately re-trips Open. In Open, refreshes
    /// opened_at so a sustained failure burst does not let a stale timestamp
    /// open the gate prematurely.
    fn record_failure(&mut self, now: Instant) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        match self.state {
            BreakerState::Closed => {
                if self.consecutive_failures >= self.config.failure_threshold {
                    self.state = BreakerState::Open;
                    self.opened_at = Some(now);
                }
            }
            BreakerState::HalfOpen => {
                self.state = BreakerState::Open;
                self.opened_at = Some(now);
            }
            BreakerState::Open => {
                self.opened_at = Some(now);
            }
        }
    }

    /// Record a successful call. Any state returns to Closed with the failure
    /// count reset; a single success in HalfOpen is the proof that the
    /// downstream has recovered.
    fn record_success(&mut self) {
        self.state = BreakerState::Closed;
        self.consecutive_failures = 0;
        self.opened_at = None;
    }

    /// Remaining cool-down when Open and the cool-down has not yet elapsed;
    /// None otherwise (Closed, HalfOpen, or past the cool-down). Read-only: it
    /// does NOT transition Open to HalfOpen the way allow() does, so a status
    /// read does not probe.
    fn cool_down_remaining(&self, now: Instant) -> Option<Duration> {
        let opened = self.opened_at?;
        if self.state != BreakerState::Open {
            return None;
        }
        let elapsed = now.duration_since(opened);
        if elapsed >= self.config.cool_down {
            None
        } else {
            Some(self.config.cool_down - elapsed)
        }
    }
}

/// A thread-safe circuit breaker. The inner Mutex is held only inside
/// allow/record_* and never across an await point, so the breaker is safe to
/// share across async tasks via Arc<CircuitBreaker>.
pub struct CircuitBreaker {
    inner: Mutex<BreakerInner>,
}

impl CircuitBreaker {
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            inner: Mutex::new(BreakerInner::new(config)),
        }
    }

    /// Snapshot of the current state. Cheap; takes the lock briefly.
    pub fn state(&self) -> BreakerState {
        self.inner.lock().expect("breaker mutex").state
    }

    /// Whether a call should proceed. Transitions Open to HalfOpen when the
    /// cool-down has elapsed.
    pub fn allow(&self) -> bool {
        self.inner
            .lock()
            .expect("breaker mutex")
            .allow(Instant::now())
    }

    /// Record a failed call; may trip the breaker to Open.
    pub fn record_failure(&self) {
        self.inner
            .lock()
            .expect("breaker mutex")
            .record_failure(Instant::now());
    }

    /// Record a successful call; resets to Closed.
    pub fn record_success(&self) {
        self.inner.lock().expect("breaker mutex").record_success();
    }

    /// Remaining cool-down when Open and the cool-down has not elapsed; None
    /// otherwise. Read-only: does not transition Open to HalfOpen the way
    /// allow() does. Surfaced for status reads (a /sandbox countdown) without
    /// side-effecting the breaker.
    pub fn cool_down_remaining(&self) -> Option<Duration> {
        self.inner
            .lock()
            .expect("breaker mutex")
            .cool_down_remaining(Instant::now())
    }
}

/// The outcome of a retry loop guarded by a circuit breaker. Wraps RetryError
/// so existing RetryError consumers are unaffected.
#[derive(Debug)]
pub enum GuardedError<E> {
    /// The retry loop ran and produced its own outcome.
    Retry(RetryError<E>),
    /// The breaker was Open and the call was rejected without an attempt.
    BreakerOpen,
}

impl<E: Display> Display for GuardedError<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retry(e) => write!(f, "{e}"),
            Self::BreakerOpen => write!(f, "circuit breaker open; call rejected"),
        }
    }
}

impl<E: Debug + Display> Error for GuardedError<E> {}

impl Retry {
    /// Run an operation with exponential backoff, retrying only errors the
    /// is_retryable predicate accepts, behind a circuit breaker. Before each
    /// attempt the breaker is consulted; if it rejects, the loop returns
    /// GuardedError::BreakerOpen without consuming an attempt. Success records
    /// to the breaker (resetting it); failure records and may trip it. Use
    /// this variant for dead-loop-prone calls such as auto compact.
    pub async fn run_guarded<F, Fut, T, E, P>(
        &self,
        mut operation: F,
        is_retryable: P,
        breaker: &CircuitBreaker,
    ) -> Result<T, GuardedError<E>>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Debug,
        P: Fn(&E) -> bool,
    {
        let mut delay = self.base_delay;
        let mut last_err: Option<E> = None;
        for attempt in 1..=self.max_attempts {
            if !breaker.allow() {
                return Err(GuardedError::BreakerOpen);
            }
            match operation(attempt).await {
                Ok(v) => {
                    breaker.record_success();
                    return Ok(v);
                }
                Err(e) => {
                    breaker.record_failure();
                    if !is_retryable(&e) {
                        return Err(GuardedError::Retry(RetryError::Fatal { error: e }));
                    }
                    last_err = Some(e);
                    if attempt == self.max_attempts {
                        break;
                    }
                    sleep(jittered(delay)).await;
                    delay = (delay * 2).min(self.max_delay);
                }
            }
        }
        Err(GuardedError::Retry(RetryError::Exhausted {
            attempts: self.max_attempts,
            last: last_err.expect("at least one attempt ran"),
        }))
    }
}

pub mod resource_breaker;

#[cfg(test)]
mod tests;
