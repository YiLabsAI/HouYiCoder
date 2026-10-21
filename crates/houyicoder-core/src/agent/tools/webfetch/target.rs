//! What a fetch is allowed to reach: the URL rule, the numeric-host guard,
//! and the resolved-address screen each hop passes before it is sent.

use super::*;
use reqwest::Url;
use std::net::IpAddr;

/// Maximum redirect hops followed. Guards against redirect loops. A hop may
/// cross hosts, but every target still passes the same URL check, so a
/// redirect cannot reach a private address the first hop was denied.
pub(crate) const MAX_REDIRECTS: usize = 10;

/// What a response means for the redirect chain.
pub(crate) enum Hop {
    /// The response is the answer.
    Done,
    /// The next target to request.
    Follow(String),
    /// The payload that refuses the hop.
    Refuse(Value),
}

/// Decide the next hop from a response. The hop count is capped and the target
/// passes the same URL rule as the first request; its resolved addresses are
/// screened by the client resolver.
pub(crate) fn redirect_hop(current: &str, location: Option<&str>, hops: usize) -> Hop {
    let Some(location) = location else {
        return Hop::Done;
    };
    if hops >= MAX_REDIRECTS {
        return Hop::Refuse(json!({
            "error": "webfetch: too many redirects",
            "error_kind": "redirect",
        }));
    }
    let base = match Url::parse(current) {
        Ok(base) => base,
        Err(e) => {
            return Hop::Refuse(json!({
                "error": format!("webfetch: bad redirect base: {e}"),
                "error_kind": "redirect",
            }));
        }
    };
    let next = match base.join(location) {
        Ok(next) => next,
        Err(e) => {
            return Hop::Refuse(json!({
                "error": format!("webfetch: bad redirect target: {e}"),
                "error_kind": "redirect",
            }));
        }
    };
    if !validate_url(next.as_str()) {
        return Hop::Refuse(json!({
            "error": format!("webfetch: redirect target rejected: {next}"),
            "error_kind": "redirect",
        }));
    }
    Hop::Follow(next.to_string())
}

/// Why a screened request stopped: a refusal the model reads as a payload, or
/// a transport failure that names its stage.
#[derive(Debug)]
pub(crate) enum FetchError {
    Refused(Value),
    Transport(reqwest::Error),
}

/// Request a target, following redirects by hand so every hop passes the URL
/// rule before it is sent. The resolved addresses are screened by the client's
/// resolver, which is also the lookup the connection uses. Each hop carries
/// the request timeout on its own, so a long chain can spend more than one
/// request budget; the hop count is what bounds it.
pub(crate) async fn fetch_following(
    client: &Client,
    start: &str,
) -> Result<reqwest::Response, FetchError> {
    let mut target = start.to_string();
    let mut hops = 0usize;
    loop {
        let response = client
            .get(&target)
            .header("Accept", "text/markdown, text/html, */*")
            .header("User-Agent", "webfetch/0.1")
            .send()
            .await;
        let Ok(response) = response else {
            return response.map_err(FetchError::Transport);
        };
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .filter(|_| response.status().is_redirection());
        match redirect_hop(&target, location, hops) {
            Hop::Done => return Ok(response),
            Hop::Follow(next) => {
                target = next;
                hops += 1;
            }
            Hop::Refuse(payload) => return Err(FetchError::Refused(payload)),
        }
    }
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
