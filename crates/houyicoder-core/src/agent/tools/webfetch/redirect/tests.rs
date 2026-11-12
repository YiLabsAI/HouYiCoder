//! Tests for redirect following, proxy-to-direct fallback, and transient retry.
//!
//! The transport cases run against a loopback mock server, never the public
//! network: the client maps the public-looking name onto that server, so the
//! URL keeps the shape the follower screens while the socket stays local.

use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::Duration;

use houyicoder_platform::host::net::ProxySource;
use reqwest::Client;
use reqwest::Proxy;
use reqwest::redirect::Policy;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::runtime::Builder;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::FetchError;
use super::MAX_REDIRECTS;
use super::Redirect;
use super::fetch_following;
use super::is_connection_refused;
use super::is_local_proxy_refusal;
use super::next_redirect;
use super::send_request;
use crate::agent::tools::webfetch::{ProxyClient, read_body};

/// A client that sends the public-looking test name to the loopback server,
/// with redirects left to the follower under test.
fn test_client(addr: SocketAddr) -> Client {
    Client::builder()
        .resolve("example.test", addr)
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .expect("client")
}

fn proxy_client(source: ProxySource, client: Client, loopback: bool) -> ProxyClient {
    ProxyClient::new(client, source, loopback)
}

/// A wrapper whose source is an I/O error, so a test can build the nested
/// chain a real transport error carries without constructing a reqwest error.
#[derive(Debug)]
struct WrappedIo(IoError);

impl Display for WrappedIo {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "wrapped io error")
    }
}

impl Error for WrappedIo {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// A loopback system proxy falls back when the connection was refused or when
/// it timed out on connect, because Windows surfaces a loopback refusal as a
/// connect timeout rather than a refusal. A TLS handshake failure stays
/// non-refusal so it never falls back. The wrapped chain proves the kind is
/// read past the top error.
#[test]
fn test_loopback_timeout_is_refusal() {
    let refused = IoError::from(ErrorKind::ConnectionRefused);
    assert!(is_local_proxy_refusal(&refused));
    assert!(is_connection_refused(&refused));

    let timed_out = IoError::from(ErrorKind::TimedOut);
    assert!(is_local_proxy_refusal(&timed_out));
    assert!(!is_connection_refused(&timed_out));

    let handshake = IoError::new(ErrorKind::UnexpectedEof, "tls handshake");
    assert!(!is_local_proxy_refusal(&handshake));
    assert!(!is_connection_refused(&handshake));

    let wrapped = WrappedIo(IoError::from(ErrorKind::TimedOut));
    assert!(is_local_proxy_refusal(&wrapped));
    assert!(!is_connection_refused(&wrapped));
}

/// A redirect is followed only while the count is under the cap and the target
/// passes the literal rule; a malformed base or target is refused with a
/// payload the model can read.
#[test]
fn test_next_redirect_decides() {
    assert!(matches!(
        next_redirect("https://example.com/a", None, 0),
        Redirect::Done
    ));
    match next_redirect("https://example.com/a", Some("/b"), 0) {
        Redirect::Follow(next) => assert_eq!(next, "https://example.com/b"),
        _ => panic!("a relative redirect is followed"),
    }
    assert!(matches!(
        next_redirect("https://example.com/a", Some("/b"), MAX_REDIRECTS),
        Redirect::Refuse(_)
    ));
    assert!(matches!(
        next_redirect("https://example.com/a", Some("https://127.0.0.1/b"), 1),
        Redirect::Refuse(_)
    ));
    assert!(matches!(
        next_redirect("not a url", Some("/b"), 1),
        Redirect::Refuse(_)
    ));
    assert!(matches!(
        next_redirect("https://example.com/a", Some("http://[bad"), 1),
        Redirect::Refuse(_)
    ));
}

/// The follower screens every request: a redirect to a target the URL rule
/// refuses is refused before it is sent.
#[test]
fn test_next_redirect_refused() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "http://127.0.0.1:1/private"),
            )
            .mount(&server)
            .await;
        let client = test_client(*server.address());
        let url = format!("http://example.test:{}/start", server.address().port());
        let refused = fetch_following(&client, None, None, &url, &mut false)
            .await
            .expect_err("the redirect is refused");
        match refused {
            FetchError::Refused(payload) => assert_eq!(payload["error_kind"], "redirect"),
            FetchError::Transport(error) => {
                panic!("the request was sent instead of refused: {error}")
            }
        }
    });
}

/// A relative redirect is followed to the answer.
#[test]
fn test_follows_relative_redirect() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/second"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/second"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let client = test_client(*server.address());
        let url = format!("http://example.test:{}/start", server.address().port());
        let response = fetch_following(&client, None, None, &url, &mut false)
            .await
            .expect("response");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(read_body(response).await.expect("body"), "ok");
    });
}

/// An explicit environment proxy that fails to connect refuses to bypass the
/// configured proxy; the direct server records no request.
#[test]
fn test_env_proxy_never_direct() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("direct"))
            .mount(&server)
            .await;
        let direct = test_client(*server.address());
        let url = format!("http://example.test:{}/ok", server.address().port());
        let unreachable = Client::builder()
            .proxy(Proxy::all("http://127.0.0.1:1").expect("proxy"))
            .connect_timeout(Duration::from_millis(100))
            .build()
            .expect("proxy client");
        let env_proxy = proxy_client(ProxySource::Env, unreachable, true);
        let mut fell_back = false;
        assert!(
            fetch_following(&direct, Some(&env_proxy), None, &url, &mut fell_back)
                .await
                .is_err(),
            "explicit env proxy must not bypass a failing proxy"
        );
        assert!(!fell_back, "an env proxy never falls back");
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty(),
            "the direct server must not be reached through an env proxy"
        );
    });
}

/// A local system proxy that refuses the connection falls back to direct and
/// reports the fallback.
#[test]
fn test_system_loopback_falls_back() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("direct"))
            .mount(&server)
            .await;
        let direct = test_client(*server.address());
        let url = format!("http://example.test:{}/ok", server.address().port());
        let unreachable = Client::builder()
            .proxy(Proxy::all("http://127.0.0.1:1").expect("proxy"))
            .connect_timeout(Duration::from_millis(100))
            .build()
            .expect("proxy client");
        let system_proxy = proxy_client(ProxySource::System, unreachable, true);
        let mut fell_back = false;
        let response = fetch_following(&direct, Some(&system_proxy), None, &url, &mut fell_back)
            .await
            .expect("local proxy refusal falls back to direct");
        assert!(fell_back, "the caller learns the fallback happened");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(read_body(response).await.expect("body"), "direct");
    });
}

/// A non-local system proxy that is unreachable must never fall back: the
/// direct server records no request.
#[test]
fn test_system_remote_no_fallback() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("direct"))
            .mount(&server)
            .await;
        let direct = test_client(*server.address());
        let url = format!("http://example.test:{}/ok", server.address().port());
        let gateway = Client::builder()
            .proxy(Proxy::all("http://192.0.2.1:9").expect("proxy"))
            .connect_timeout(Duration::from_millis(100))
            .build()
            .expect("proxy client");
        let system_proxy = proxy_client(ProxySource::System, gateway, false);
        let mut fell_back = false;
        assert!(
            fetch_following(&direct, Some(&system_proxy), None, &url, &mut fell_back)
                .await
                .is_err(),
            "an unreachable enterprise gateway fails closed"
        );
        assert!(!fell_back, "a remote proxy never falls back");
        assert!(
            server
                .received_requests()
                .await
                .expect("requests")
                .is_empty(),
            "the direct server must not be reached around a remote proxy"
        );
    });
}

/// A proxy configured for TLS only sends a plain HTTP target direct by the
/// standard semantics, so a local model endpoint stays reachable when only the
/// TLS proxy is set.
#[test]
fn test_https_proxy_direct_http() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("answer"))
            .mount(&server)
            .await;
        let direct = test_client(*server.address());
        let url = format!("http://example.test:{}/ok", server.address().port());
        let proxy_http = Client::builder()
            .resolve("example.test", *server.address())
            .proxy(Proxy::https("http://127.0.0.1:1").expect("proxy"))
            .connect_timeout(Duration::from_millis(100))
            .build()
            .expect("proxy client");
        let system_proxy = proxy_client(ProxySource::System, proxy_http, true);
        let mut fell_back = false;
        let response = fetch_following(&direct, Some(&system_proxy), None, &url, &mut fell_back)
            .await
            .expect("a plain target with a TLS-only proxy goes direct");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(read_body(response).await.expect("body"), "answer");
        assert!(
            !fell_back,
            "direct routing for an unconfigured scheme is not a fallback"
        );
    });
}

/// A proxy that answers with an error status returns that response; the
/// exchange completed, so the direct client is never used. The same branch
/// covers every HTTP status a proxy returns, including a 502 from the proxy
/// itself.
#[test]
fn test_proxy_status_no_fallback() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let proxy = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(407))
            .mount(&proxy)
            .await;
        let direct = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("direct"))
            .mount(&direct)
            .await;
        let direct_client = test_client(*direct.address());
        let proxy_url = format!("http://{}", proxy.address());
        let proxy_http = Client::builder()
            .proxy(Proxy::all(&proxy_url).expect("proxy"))
            .build()
            .expect("proxy client");
        let system_proxy = proxy_client(ProxySource::System, proxy_http, true);
        let mut fell_back = false;
        let response = fetch_following(
            &direct_client,
            Some(&system_proxy),
            None,
            "http://example.test/secure",
            &mut fell_back,
        )
        .await
        .expect("a proxy status is a completed exchange");
        assert_eq!(response.status().as_u16(), 407);
        assert!(!fell_back, "an error status is not a refused connection");
        assert!(
            direct
                .received_requests()
                .await
                .expect("requests")
                .is_empty(),
            "the direct client must not be used after a proxy status"
        );
    });
}

/// A local proxy whose TLS handshake fails must not be bypassed, even though
/// the endpoint is on this machine.
#[test]
fn test_proxy_tls_no_fallback() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("listener");
        let port = listener.local_addr().expect("address").port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                drop(socket.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await);
                drop(socket.shutdown().await);
            }
        });
        let direct = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("direct"))
            .mount(&direct)
            .await;
        let direct_client = test_client(*direct.address());
        let proxy_url = format!("https://127.0.0.1:{port}");
        let proxy_http = Client::builder()
            .proxy(Proxy::all(&proxy_url).expect("proxy"))
            .connect_timeout(Duration::from_millis(500))
            .build()
            .expect("proxy client");
        let system_proxy = proxy_client(ProxySource::System, proxy_http, true);
        let mut fell_back = false;
        match send_request(
            &direct_client,
            Some(&system_proxy),
            "http://example.test/ok",
            &mut fell_back,
        )
        .await
        {
            Err(FetchError::Transport(error)) => {
                // The failure is connect-class; the local-proxy-refusal
                // check keeps it from falling back.
                assert!(
                    error.is_connect(),
                    "the handshake failure must stay connect-class for this test to discriminate"
                );
                assert!(
                    !is_connection_refused(&error),
                    "a TLS handshake failure is not a refused connection"
                );
            }
            other => {
                panic!("the TLS handshake failure must surface as a transport error: {other:?}")
            }
        }
        assert!(
            !fell_back,
            "a connect refusal or loopback timeout may fall back, not a TLS handshake failure"
        );
        assert!(
            direct
                .received_requests()
                .await
                .expect("requests")
                .is_empty(),
            "the direct client must not be used after a TLS failure"
        );
    });
}

/// A direct connect failure retries once and then returns the error.
#[test]
fn test_direct_connect_retries() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let closed = Client::builder()
            .connect_timeout(Duration::from_millis(50))
            .no_proxy()
            .build()
            .expect("closed client");
        assert!(
            fetch_following(&closed, None, None, "http://127.0.0.1:1/closed", &mut false)
                .await
                .is_err(),
            "direct connect failure retries once then returns error"
        );
    });
}
