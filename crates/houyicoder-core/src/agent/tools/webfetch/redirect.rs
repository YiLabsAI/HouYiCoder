//! What a fetch is allowed to reach: the URL rule, the numeric-host guard,
//! and the resolved-address screen each hop passes before it is sent.

use super::*;
use reqwest::Url;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::time::Duration;
use tokio::time::sleep;

/// Maximum redirect hops followed. Guards against redirect loops. A hop may
/// cross hosts, but every target still passes the same URL check, so a
/// redirect cannot reach a private address the first hop was denied.
pub(crate) const MAX_REDIRECTS: usize = 10;

/// What a response means for the redirect chain.
pub(crate) enum Redirect {
    /// The response is the answer.
    Done,
    /// The next target to request.
    Follow(String),
    /// The payload that refuses the redirect.
    Refuse(Value),
}

/// Decide the next redirect from a response. The redirect count is capped and
/// the target passes the same URL rule as the first request; its resolved
/// addresses are screened by the client resolver.
pub(crate) fn next_redirect(current: &str, location: Option<&str>, redirects: usize) -> Redirect {
    let Some(location) = location else {
        return Redirect::Done;
    };
    if redirects >= MAX_REDIRECTS {
        return Redirect::Refuse(json!({
            "error": "webfetch: too many redirects",
            "error_kind": "redirect",
        }));
    }
    let base = match Url::parse(current) {
        Ok(base) => base,
        Err(e) => {
            return Redirect::Refuse(json!({
                "error": format!("webfetch: bad redirect base: {e}"),
                "error_kind": "redirect",
            }));
        }
    };
    let next = match base.join(location) {
        Ok(next) => next,
        Err(e) => {
            return Redirect::Refuse(json!({
                "error": format!("webfetch: bad redirect target: {e}"),
                "error_kind": "redirect",
            }));
        }
    };
    if !validate_url(next.as_str()) {
        return Redirect::Refuse(json!({
            "error": format!("webfetch: redirect target rejected: {next}"),
            "error_kind": "redirect",
        }));
    }
    Redirect::Follow(next.to_string())
}

/// Why a screened request stopped: a refusal the model reads as a payload, or
/// a transport failure that names its stage.
#[derive(Debug)]
pub(crate) enum FetchError {
    Refused(Value),
    Transport(reqwest::Error),
}

/// Request a target, following redirects by hand so every request passes the
/// URL rule and the resolved-address screen before it is sent.
///
/// An explicit environment proxy is authoritative and never falls back. A
/// desktop system proxy falls back only when a local proxy refused the
/// connection itself, and the caller learns that a fallback happened through
/// fell_back so it can warn and bound the window.
pub(crate) async fn fetch_following(
    direct: &Client,
    proxy: Option<&ProxyClient>,
    resolver: Option<&PublicResolver>,
    start: &str,
    fell_back: &mut bool,
) -> Result<reqwest::Response, FetchError> {
    let mut target = start.to_string();
    let mut redirects = 0usize;
    loop {
        if let Some(res) = resolver {
            res.screen_target(&target)
                .await
                .map_err(FetchError::Refused)?;
        }
        let response = send_request(direct, proxy, &target, fell_back).await?;
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .filter(|_| response.status().is_redirection());
        match next_redirect(&target, location, redirects) {
            Redirect::Done => return Ok(response),
            Redirect::Follow(next) => {
                target = next;
                redirects += 1;
            }
            Redirect::Refuse(payload) => return Err(FetchError::Refused(payload)),
        }
    }
}

/// Send one GET request according to the proxy policy. A proxy client routes
/// each scheme through its configured endpoint and sends an unconfigured
/// scheme direct, so a target whose scheme has no endpoint is never refused
/// here. The direct client is used only when no proxy exists, or after a local
/// system proxy refused the connection.
async fn send_request(
    direct: &Client,
    proxy: Option<&ProxyClient>,
    target: &str,
    fell_back: &mut bool,
) -> Result<reqwest::Response, FetchError> {
    if let Some(proxy_client) = proxy {
        match send_once(&proxy_client.client, target).await {
            Ok(response) => return Ok(response),
            Err(error)
                if proxy_client.may_fall_back()
                    && error.is_connect()
                    && is_connection_refused(&error) =>
            {
                *fell_back = true;
            }
            Err(error) => return Err(FetchError::Transport(error)),
        }
    }
    direct_with_retry(direct, target)
        .await
        .map_err(FetchError::Transport)
}

/// Whether the transport chain reached an actual connection refusal. A proxy
/// fails closed on every other transport fault, including a timeout and a TLS
/// handshake failure.
fn is_connection_refused(error: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn Error + 'static)> = error.source();
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<IoError>()
            && io.kind() == ErrorKind::ConnectionRefused
        {
            return true;
        }
        source = current.source();
    }
    false
}

/// Send direct, retrying one connect failure once after a short pause.
async fn direct_with_retry(
    direct: &Client,
    target: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    match send_once(direct, target).await {
        Ok(response) => Ok(response),
        Err(error) if error.is_connect() => {
            sleep(Duration::from_millis(50)).await;
            send_once(direct, target).await
        }
        Err(error) => Err(error),
    }
}

async fn send_once(client: &Client, target: &str) -> Result<reqwest::Response, reqwest::Error> {
    client
        .get(target)
        .header("Accept", "text/markdown, text/html, */*")
        .header("User-Agent", "webfetch/0.1")
        .send()
        .await
}

/// Validate a URL string. Rejects oversized URLs, malformed URLs, URLs
/// with username/password credentials, and hostnames without a dot (a
/// heuristic to block internal or privileged addresses).
pub(crate) fn validate_url(url: &str) -> bool {
    if url.len() > MAX_URL_LENGTH {
        return false;
    }
    let after_scheme = if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        rest
    } else {
        return false;
    };
    let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if authority.contains('@') {
        return false;
    }
    let hostname = authority.split(':').next().unwrap_or(authority);
    if !hostname.contains('.') {
        return false;
    }
    if is_private_host(hostname) {
        return false;
    }
    true
}

/// Whether a resolved address belongs to this machine or a private network.
pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // Carrier-grade NAT, not reachable from the public internet.
                || (a == 100 && (64..=127).contains(&b))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique local and link local.
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Whether a host names this machine or a private network. A public page can
/// redirect to one of these, so the same check runs on every hop.
///
/// A name is screened here by its literal form; the resolved addresses are
/// screened by the client resolver, which is the lookup the connection uses.
pub(crate) fn is_private_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".local") {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private_ip(ip);
    }
    // A host written only with digits and dots is a non-canonical IPv4
    // spelling (127.1, 0177.0.0.1) that resolvers still accept, so it is not
    // a public name.
    // Only the non-canonical IPv4 spellings are numeric hosts: all digits and
    // dots, or a hex literal marked with 0x. A word made of hex letters is a
    // real domain name and stays allowed.
    host.chars().all(|c| c.is_ascii_digit() || c == '.')
        || (host.contains("0x")
            && host
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '.' || c == 'x'))
}

/// Upgrade an HTTP URL to HTTPS. Leaves HTTPS URLs unchanged.
pub(crate) fn upgrade_to_https(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests;
