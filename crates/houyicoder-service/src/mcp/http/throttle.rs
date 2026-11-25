//! The reconnect throttle in front of the HTTP client: a stream that closes
//! sooner than the stability threshold is a rapid death, and each later
//! rapid death delays the next stream with a doubling wait. A server that
//! keeps accepting and immediately dropping connections is redialled less
//! and less often instead of in a tight loop, and one warning is emitted per
//! storm.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

use futures::Stream;
use futures::stream::BoxStream;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::StreamableHttpClient;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use rmcp::transport::streamable_http_client::StreamableHttpPostResponse;
use rmcp::transport::streamable_http_client::StreamableHttpResponseLimits;
use sse_stream::Error as SseError;
use sse_stream::Sse;
use tokio::time::sleep;

/// How short a stream's life must be to count as a rapid death, the first
/// delayed redial, and the longest delayed redial.
#[derive(Debug, Clone)]
pub(crate) struct ThrottlePolicy {
    pub rapid: Duration,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for ThrottlePolicy {
    fn default() -> Self {
        Self {
            rapid: Duration::from_secs(2),
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
        }
    }
}

/// What the throttle remembers between streams.
#[derive(Debug, Default)]
struct ThrottleState {
    rapid_deaths: u32,
    warned: bool,
}

/// An HTTP client that paces redials after rapid stream deaths. Every other
/// call is the inner client's; only the streams it opens are wrapped, so a
/// closed stream is what feeds the counter.
pub(crate) struct ThrottledHttpClient<C> {
    inner: C,
    state: Arc<Mutex<ThrottleState>>,
    policy: ThrottlePolicy,
}

impl<C: StreamableHttpClient + Sync> ThrottledHttpClient<C> {
    /// Wrap one client with the default policy.
    pub(crate) fn new(inner: C) -> Self {
        Self::with_policy(inner, ThrottlePolicy::default())
    }

    /// Wrap one client with an explicit policy, so a test can use
    /// millisecond windows.
    pub(crate) fn with_policy(inner: C, policy: ThrottlePolicy) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(ThrottleState::default())),
            policy,
        }
    }

    /// Wait out a delay when the last streams died quickly. The first rapid
    /// death does not delay; each later one doubles the base from there.
    async fn pace(&self) {
        let delay = {
            let state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            delay_for(&state, &self.policy)
        };
        if let Some(delay) = delay {
            sleep(delay).await;
        }
    }

    /// Wrap one stream so its lifetime reaches the counter.
    fn track(
        &self,
        stream: BoxStream<'static, Result<Sse, SseError>>,
    ) -> BoxStream<'static, Result<Sse, SseError>> {
        Box::pin(TrackedStream {
            inner: stream,
            state: Arc::clone(&self.state),
            policy: self.policy.clone(),
            opened: Instant::now(),
            settled: false,
            delivered: false,
        })
    }

    /// Wrap a post response whose body is an SSE stream; other shapes are the
    /// inner client's answer unchanged.
    fn track_response(&self, response: StreamableHttpPostResponse) -> StreamableHttpPostResponse {
        match response {
            StreamableHttpPostResponse::Sse(stream, session) => {
                StreamableHttpPostResponse::Sse(self.track(stream), session)
            }
            other => other,
        }
    }
}

impl<C: StreamableHttpClient + Sync> Clone for ThrottledHttpClient<C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            state: Arc::clone(&self.state),
            policy: self.policy.clone(),
        }
    }
}

impl<C: StreamableHttpClient + Sync> StreamableHttpClient for ThrottledHttpClient<C> {
    type Error = C::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.pace().await;
        let response = self
            .inner
            .post_message(uri, message, session_id, auth_header, custom_headers)
            .await?;
        Ok(self.track_response(response))
    }

    async fn post_message_with_response_limits(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        limits: StreamableHttpResponseLimits,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.pace().await;
        let response = self
            .inner
            .post_message_with_response_limits(
                uri,
                message,
                session_id,
                auth_header,
                custom_headers,
                limits,
            )
            .await?;
        Ok(self.track_response(response))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.inner
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.pace().await;
        let stream = self
            .inner
            .get_stream(uri, session_id, last_event_id, auth_header, custom_headers)
            .await?;
        Ok(self.track(stream))
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        self.pace().await;
        let stream = self
            .inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await?;
        Ok(self.track(stream))
    }
}

/// One tracked stream: it reports its lifetime once, whether it ends or is
/// dropped. A stream that delivered an event did its job, so only a stream
/// that ends without one counts against the server: a request-scoped
/// response stream is meant to close after its answer, while a stream that
/// opened and produced nothing is the failure the pacing exists for.
struct TrackedStream {
    inner: BoxStream<'static, Result<Sse, SseError>>,
    state: Arc<Mutex<ThrottleState>>,
    policy: ThrottlePolicy,
    opened: Instant,
    settled: bool,
    delivered: bool,
}

impl Stream for TrackedStream {
    type Item = Result<Sse, SseError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.inner.as_mut().poll_next(cx);
        if let Poll::Ready(Some(Ok(_))) = &polled {
            self.delivered = true;
        }
        if matches!(polled, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            let opened = self.opened;
            let delivered = self.delivered;
            if !self.settled {
                self.settled = true;
                settle_locked(&self.state, &self.policy, opened, delivered);
            }
        }
        polled
    }
}

impl Drop for TrackedStream {
    fn drop(&mut self) {
        if !self.settled {
            settle_locked(&self.state, &self.policy, self.opened, self.delivered);
        }
    }
}

/// The delay the throttle waits before the next redial, as a pure function
/// of the state and the policy. The first rapid death is free; from the
/// second the base doubles per death up to the cap.
fn delay_for(state: &ThrottleState, policy: &ThrottlePolicy) -> Option<Duration> {
    if state.rapid_deaths < 2 {
        None
    } else {
        let step = state.rapid_deaths.saturating_sub(2);
        Some(
            policy
                .base
                .saturating_mul(2u32.saturating_pow(step))
                .min(policy.cap),
        )
    }
}

/// The settle rules, shared by the ending stream and the dropped one.
/// A stream that delivered an event is a healthy answer: it resets the
/// storm regardless of how long it lived. An empty stream that also
/// closed inside the threshold increments the death counter; anything
/// else resets.
fn settle_locked(
    state: &Arc<Mutex<ThrottleState>>,
    policy: &ThrottlePolicy,
    opened: Instant,
    delivered: bool,
) {
    let mut state = match state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    if delivered || opened.elapsed() >= policy.rapid {
        state.rapid_deaths = 0;
        state.warned = false;
    } else {
        state.rapid_deaths = state.rapid_deaths.saturating_add(1);
        if !state.warned {
            state.warned = true;
            tracing::warn!(
                rapid_ms = policy.rapid.as_millis() as u64,
                "an mcp http stream keeps closing right after it opens; later redials are delayed"
            );
        }
    }
}

#[cfg(test)]
mod tests;
