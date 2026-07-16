//! The network segment of the Seatbelt profile.
//!
//! Split out of the profile module because the kernel's network filters carry
//! their own set of non-obvious behaviours, documented on each builder below.
//! Two of them are traps where the natural spelling of a rule silently produces
//! a much wider fence than it reads as, which is why the reasoning is recorded
//! next to the emitted text: a reader changing these rules needs it here.

use houyicoder_api::sandbox::{NetworkPolicy, UnixSockets};

/// Quote a path as an SBPL string literal, including the surrounding quotes.
///
/// Config-supplied paths are the only externally controlled strings that reach
/// the profile text, so they cannot be interpolated raw: an embedded quote would
/// close the literal early and let the remainder of the path be parsed as
/// profile syntax, which is a rule-injection hole rather than a cosmetic bug.
/// Backslash is escaped before the quote so an input ending in a backslash
/// cannot escape the closing quote.
fn sbpl_string(path: &str) -> String {
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The network segment of the profile.
///
/// Two shapes, selected by the egress posture. Unrestricted emits a blanket
/// allow of the network operation class (the open profile). Denied emits the
/// deny of that class followed by the local allow-back rules, in that order
/// because seatbelt is last-match-wins and each allow-back has to land after
/// the deny it carves out of.
///
/// Both unix sockets and loopback binding are local IPC, not egress, so they are
/// available in the denied posture. They need explicit allow-backs there only
/// because the kernel files them under the same operation class as egress, so
/// the class deny sweeps them up as collateral.
pub(crate) fn network_rules(policy: &NetworkPolicy, tag: &str) -> String {
    if policy.allows_egress() {
        // Stated as a whole-class allow rather than a set of narrower rules,
        // because that is what it is. See NetworkPolicy for why no port-scoped
        // tier sits between this and the denied posture.
        return "(allow network*)\n".to_string();
    }
    let mut s = format!("(deny network* (with message \"{tag}\"))\n");
    if policy.allow_local_binding {
        s.push_str(&allow_local_binding_rules());
    }
    s.push_str(&allow_unix_socket_rules(&policy.unix_sockets));
    s
}

/// Allow-back rules for binding and accepting on loopback.
///
/// Two kernel behaviours drive the exact filters here, and both are traps that
/// a plain reading of the profile language leads you into.
///
/// Bind and inbound use a wildcard host rather than the loopback host token.
/// A modern runtime opens a dual-stack socket by default, so binding it to the
/// v4 loopback address is represented in the kernel as the v4-mapped v6 form,
/// which the loopback host token does not match. The language accepts only that
/// token or the wildcard as a host, so the wildcard is the only spelling that
/// admits the mapped form. Wildcarding is safe for these two operations because
/// neither has a remote endpoint, so neither can grant egress.
///
/// Outbound instead filters on the remote address, and must. A local-address
/// filter on outbound is evaluated against the source address, which for a
/// socket that has not been bound is the any-address at connect time — and the
/// loopback token matches the any-address, so a local-address filter on outbound
/// admits every outbound connection to every host. Writing the loopback
/// allow-back that way would silently convert the contained posture into an open
/// one, which is exactly the class of mistake the kernel probe exists to catch.
fn allow_local_binding_rules() -> String {
    "(allow network-bind (local ip \"*:*\"))\n\
     (allow network-inbound (local ip \"*:*\"))\n\
     (allow network-outbound (remote ip \"localhost:*\"))\n"
        .to_string()
}

/// Allow-back rules for unix domain socket IPC.
///
/// Three operations must be allowed for a socket to be usable, and missing any
/// one of them fails the whole path. Creating the socket is a separate operation
/// from using it and carries no path, so it cannot be covered by a path filter
/// and needs its own rule keyed on the socket domain. Binding filters on the
/// local path, connecting filters on the remote path.
///
/// A relative path is dropped rather than emitted. The profile language matches
/// absolute paths, so a relative entry would render a rule that can never fire —
/// a config that reads as effective while being inert. Dropping it keeps the
/// posture narrow; the mapping from settings to policy is where the user is told
/// the entry was rejected.
fn allow_unix_socket_rules(sockets: &UnixSockets) -> String {
    match sockets {
        UnixSockets::Denied => String::new(),
        UnixSockets::All => "(allow system-socket (socket-domain AF_UNIX))\n\
             (allow network-bind (local unix-socket (path-regex #\"^/\")))\n\
             (allow network-outbound (remote unix-socket (path-regex #\"^/\")))\n"
            .to_string(),
        UnixSockets::Paths(paths) => {
            let absolute: Vec<&String> = paths.iter().filter(|p| p.starts_with('/')).collect();
            if absolute.is_empty() {
                return String::new();
            }
            let mut s = "(allow system-socket (socket-domain AF_UNIX))\n".to_string();
            for p in absolute {
                let q = sbpl_string(p);
                s.push_str(&format!(
                    "(allow network-bind (local unix-socket (subpath {q})))\n"
                ));
                s.push_str(&format!(
                    "(allow network-outbound (remote unix-socket (subpath {q})))\n"
                ));
            }
            s
        }
    }
}

#[cfg(test)]
#[path = "network_tests.rs"]
mod network_tests;
