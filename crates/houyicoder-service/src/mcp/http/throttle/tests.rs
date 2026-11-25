//! Tests for the reconnect throttle: the delay schedule and the death
//! counter are pure functions, so the tests call them directly without
//! sleeping on a wall clock.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use futures::StreamExt;
use futures::stream::BoxStream;
use futures::stream::once;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::StreamableHttpClient;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use rmcp::transport::streamable_http_client::StreamableHttpPostResponse;
use sse_stream::Error as SseError;
use sse_stream::Sse;

use super::*;

fn policy() -> ThrottlePolicy {
    ThrottlePolicy {
        rapid: Duration::from_secs(2),
        base: Duration::from_millis(500),
        cap: Duration::from_secs(30),
    }
}

fn state(deaths: u32, warned: bool) -> ThrottleState {
    ThrottleState {
        rapid_deaths: deaths,
        warned,
    }
}

/// Zero or one rapid death does not delay the next redial; the delay
/// starts from the second death and doubles each step.
#[test]
fn test_delay_schedule() {
    let p = policy();
    assert_eq!(delay_for(&state(0, false), &p), None);
    assert_eq!(delay_for(&state(1, true), &p), None);
    assert_eq!(
        delay_for(&state(2, true), &p),
        Some(Duration::from_millis(500))
    );
    assert_eq!(delay_for(&state(3, true), &p), Some(Duration::from_secs(1)));
    assert_eq!(delay_for(&state(4, true), &p), Some(Duration::from_secs(2)));
}

/// The cap stops the doubling no matter how many deaths pile up.
#[test]
fn test_delay_caps() {
    let p = ThrottlePolicy {
        rapid: Duration::from_millis(20),
        base: Duration::from_millis(10),
        cap: Duration::from_millis(40),
    };
    assert_eq!(
        delay_for(&state(10, true), &p),
        Some(Duration::from_millis(40))
    );
    assert_eq!(
        delay_for(&state(100, true), &p),
        Some(Duration::from_millis(40))
    );
}

/// An empty stream that closed inside the threshold counts as a death and
/// fires the warning once; a stream that delivered an event resets both.
#[test]
fn test_settle_counts_and_resets() {
    let p = ThrottlePolicy {
        rapid: Duration::from_millis(20),
        base: Duration::from_millis(10),
        cap: Duration::from_millis(40),
    };
    let s = Arc::new(Mutex::new(ThrottleState::default()));

    settle_locked(&s, &p, Instant::now(), false);
    assert_eq!(s.lock().unwrap().rapid_deaths, 1);
    assert!(s.lock().unwrap().warned);

    settle_locked(&s, &p, Instant::now(), false);
    assert_eq!(s.lock().unwrap().rapid_deaths, 2);
    assert!(s.lock().unwrap().warned);

    settle_locked(&s, &p, Instant::now(), true);
    assert_eq!(s.lock().unwrap().rapid_deaths, 0);
    assert!(!s.lock().unwrap().warned);
}

/// A stream that closed slowly but still empty resets the counter, because
/// a long pause means the server recovered, not that it flapped.
#[test]
fn test_slow_empty_resets() {
    let p = ThrottlePolicy {
        rapid: Duration::from_millis(20),
        base: Duration::from_millis(10),
        cap: Duration::from_millis(40),
    };
    let s = Arc::new(Mutex::new(state(5, true)));
    let long_ago = Instant::now() - Duration::from_secs(10);
    settle_locked(&s, &p, long_ago, false);
    assert_eq!(s.lock().unwrap().rapid_deaths, 0);
    assert!(!s.lock().unwrap().warned);
}

/// The default policy names the production windows.
#[test]
fn test_default_policy() {
    let p = ThrottlePolicy::default();
    assert_eq!(p.rapid, Duration::from_secs(2));
    assert_eq!(p.base, Duration::from_millis(500));
    assert_eq!(p.cap, Duration::from_secs(30));
}

// ---- delegation smoke (no timing assertions) ----

/// A client whose streams deliver one event, for verifying that non-stream
/// calls and streaming calls both reach the inner client unchanged.
#[derive(Clone, Copy)]
struct StreamClient {
    post_with_sse: bool,
}

impl StreamableHttpClient for StreamClient {
    type Error = std::io::Error;

    async fn post_message(
        &self,
        _uri: Arc<str>,
        _message: ClientJsonRpcMessage,
        _session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        if self.post_with_sse {
            Ok(StreamableHttpPostResponse::Sse(
                once(async {
                    Ok(Sse {
                        event: None,
                        data: Some("{}".into()),
                        id: None,
                        retry: None,
                    })
                })
                .boxed(),
                None,
            ))
        } else {
            Ok(StreamableHttpPostResponse::Accepted)
        }
    }

    async fn delete_session(
        &self,
        _uri: Arc<str>,
        _session_id: Arc<str>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        Ok(())
    }

    async fn get_stream(
        &self,
        _uri: Arc<str>,
        _session_id: Option<Arc<str>>,
        _last_event_id: Option<String>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        Ok(once(async {
            Ok(Sse {
                event: None,
                data: Some("{}".into()),
                id: None,
                retry: None,
            })
        })
        .boxed())
    }
}

/// Every call the throttle forwards reaches the inner client, and a POST
/// answered with a stream is wrapped but still delivers its event.
#[tokio::test]
async fn test_forwarded_calls() {
    let message: ClientJsonRpcMessage = serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }))
    .expect("a notification");
    let plain = ThrottledHttpClient::new(StreamClient {
        post_with_sse: false,
    });
    let accepted = plain
        .post_message(
            Arc::from("http://localhost/mcp"),
            message.clone(),
            None,
            None,
            HashMap::new(),
        )
        .await
        .expect("a post answers");
    assert!(matches!(accepted, StreamableHttpPostResponse::Accepted));
    plain
        .delete_session(
            Arc::from("http://localhost/mcp"),
            Arc::from("s"),
            None,
            HashMap::new(),
        )
        .await
        .expect("a delete answers");
    let stream = plain
        .get_stream(
            Arc::from("http://localhost/mcp"),
            None,
            None,
            None,
            HashMap::new(),
        )
        .await
        .expect("a stream opens");
    assert_eq!(stream.collect::<Vec<_>>().await.len(), 1);

    let sse = ThrottledHttpClient::with_policy(
        StreamClient {
            post_with_sse: true,
        },
        ThrottlePolicy {
            rapid: Duration::from_millis(20),
            base: Duration::from_millis(10),
            cap: Duration::from_millis(40),
        },
    );
    let response = sse
        .post_message(
            Arc::from("http://localhost/mcp"),
            message,
            None,
            None,
            HashMap::new(),
        )
        .await
        .expect("a streaming post answers");
    let StreamableHttpPostResponse::Sse(stream, _) = response else {
        panic!("the client answered with a stream");
    };
    assert_eq!(stream.collect::<Vec<_>>().await.len(), 1);
}
