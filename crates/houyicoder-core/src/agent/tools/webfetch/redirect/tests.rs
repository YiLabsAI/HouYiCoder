//! Tests for redirect following, proxy-to-direct fallback, and transient retry.

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::thread::sleep;
use std::thread::spawn;
use std::time::Duration;

use reqwest::Client;
use reqwest::Proxy;
use reqwest::redirect::Policy;
use tokio::runtime::Builder;

use super::FetchError;
use super::Hop;
use super::MAX_REDIRECTS;
use super::fetch_following;
use super::redirect_hop;
use crate::agent::tools::webfetch::RouteClient;
use crate::agent::tools::webfetch::read_body;

/// A hop is followed only while the count is under the cap and the target
/// passes the literal rule; a malformed base or target is refused with a
/// payload the model can read.
#[test]
fn test_redirect_hop_decides() {
    assert!(matches!(
        redirect_hop("https://example.com/a", None, 0),
        Hop::Done
    ));
    match redirect_hop("https://example.com/a", Some("/b"), 0) {
        Hop::Follow(next) => assert_eq!(next, "https://example.com/b"),
        _ => panic!("a relative hop is followed"),
    }
    assert!(matches!(
        redirect_hop("https://example.com/a", Some("/b"), MAX_REDIRECTS),
        Hop::Refuse(_)
    ));
    assert!(matches!(
        redirect_hop("https://example.com/a", Some("https://127.0.0.1/b"), 1),
        Hop::Refuse(_)
    ));
    assert!(matches!(
        redirect_hop("not a url", Some("/b"), 1),
        Hop::Refuse(_)
    ));
    assert!(matches!(
        redirect_hop("https://example.com/a", Some("http://[bad"), 1),
        Hop::Refuse(_)
    ));
}

/// The follower screens every hop: a hop to a target the URL rule refuses is
/// refused before it is sent.
#[test]
fn test_redirect_hop_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _read = stream.read(&mut buf);
        stream
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .expect("write");
        sleep(Duration::from_millis(50));
    });
    let client = Client::builder()
        .resolve("example.test", addr)
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .expect("client");
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let url = format!("http://example.test:{}/start", addr.port());
        let refused = fetch_following(&client, None, None, &url)
            .await
            .expect_err("the hop is refused");
        match refused {
            FetchError::Refused(payload) => assert_eq!(payload["error_kind"], "redirect"),
            FetchError::Transport(error) => {
                panic!("the hop was sent instead of refused: {error}")
            }
        }
    });
    server.join().expect("server");
}

/// A relative redirect is followed to the answer.
#[test]
fn test_follows_relative_redirect() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = spawn(move || {
        for hop in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 1024];
            let _read = stream.read(&mut buf);
            let reply: &[u8] = if hop == 0 {
                b"HTTP/1.1 302 Found\r\nLocation: /second\r\nContent-Length: 0\r\n\r\n"
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
            };
            stream.write_all(reply).expect("write");
        }
    });
    let client = Client::builder()
        .resolve("example.test", addr)
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .expect("client");
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let url = format!("http://example.test:{}/start", addr.port());
        let response = fetch_following(&client, None, None, &url)
            .await
            .expect("response");
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(read_body(response).await.expect("body"), "ok");
    });
    server.join().expect("server");
}

/// A desktop system route (RouteClient::System) falls back to the direct
/// client when its proxy port is closed, while an explicit environment route
/// (RouteClient::Env) refuses to bypass the configured proxy.
#[test]
fn test_fallback_to_direct() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _read = stream.read(&mut buf);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\ndirect")
            .expect("write");
    });
    let direct = Client::builder()
        .resolve("example.test", addr)
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .expect("direct client");
    let unreachable_client = Client::builder()
        .proxy(Proxy::all("http://127.0.0.1:1").expect("route"))
        .connect_timeout(Duration::from_millis(100))
        .build()
        .expect("route client");
    let env_route = RouteClient::Env(unreachable_client.clone());
    let sys_route = RouteClient::System(unreachable_client);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let url = format!("http://example.test:{}/ok", addr.port());
        assert!(
            fetch_following(&direct, Some(&env_route), None, &url)
                .await
                .is_err(),
            "explicit env route must not silently bypass a failing proxy"
        );
        let resp = fetch_following(&direct, Some(&sys_route), None, &url)
            .await
            .expect("system route falls back to direct when local port is closed");
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(read_body(resp).await.expect("body"), "direct");

        let closed_direct = Client::builder()
            .connect_timeout(Duration::from_millis(50))
            .no_proxy()
            .build()
            .expect("closed client");
        assert!(
            fetch_following(&closed_direct, None, None, "http://127.0.0.1:1/closed")
                .await
                .is_err(),
            "direct connect failure retries once then returns error"
        );
    });
    server.join().expect("server");
}
