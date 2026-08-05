//! Mapping from the on-disk sandbox settings to the enforced network policy.
//!
//! Two shapes exist on purpose. The settings shape is a serde record that must
//! stay backward compatible with files users already have; the policy shape is
//! what the fence enforces and is free to change. This module is the single
//! place they meet, which is also the single place a settings value can be
//! rejected — so rejection is reported rather than swallowed.
//!
//! Being the only mapping matters beyond tidiness. The kernel fence and the
//! permission gate both need to know the egress posture, and before this module
//! existed the gate was told it separately from the profile. Two independent
//! sources of one truth drift: a user opting into an open fence would get the
//! kernel opening while the gate still refused every egress command. Both now
//! derive from the value this module returns.

use houyicoder_api::sandbox::{Egress, NetworkPolicy, UnixSockets};
use houyicoder_config::{NetworkMode, SandboxNetworkConfig};

/// A settings entry that was not honoured, phrased for display to the user.
///
/// Carried out of the mapping rather than logged inside it so the caller decides
/// the surface (startup warning, status line, log). A rejected containment knob
/// that nobody is told about is the failure mode this type exists to prevent:
/// the user believes a socket is reachable, the fence disagrees, and the symptom
/// surfaces later as an unexplained permission error from an unrelated tool.
pub type PolicyWarning = String;

/// Map the settings record onto the enforced policy, collecting anything that
/// could not be honoured.
///
/// Every rejection narrows rather than widens: an unusable entry is dropped, not
/// approximated by something broader.
pub fn network_policy_from(cfg: &SandboxNetworkConfig) -> (NetworkPolicy, Vec<PolicyWarning>) {
    // Keys the settings format accepts but this version cannot enforce are
    // reported first, because they are the ones a user is most likely to be
    // relying on incorrectly: a destination allowlist that is not yet enforced
    // looks like protection and is not.
    let mut warnings = cfg.deferred_warnings();
    let mut policy = NetworkPolicy::contained();
    policy.egress = match cfg.mode {
        NetworkMode::Open => Egress::Unrestricted,
        // An unimplemented mode denies, same as off. The user is told about it by
        // the deferred warnings above rather than by the posture, because a
        // posture is not an explanation.
        NetworkMode::Off | NetworkMode::Unsupported(_) => Egress::Denied,
    };
    policy.allow_local_binding = cfg.allow_local_binding;
    policy.unix_sockets = map_unix_sockets(cfg, &mut warnings);
    (policy, warnings)
}

/// Resolve the two settings fields that describe unix socket access onto the
/// single enforced value.
///
/// The settings keep a blanket boolean beside an enumerated list,
/// because that is what users' files contain. The enforced
/// shape is one value, so a file that sets both has to be resolved here. The
/// blanket flag wins when both are set, but the ignored list
/// is reported: silently dropping it leaves a config that reads
/// as an allowlist while behaving as allow-everything, which is a security
/// posture the user did not choose.
fn map_unix_sockets(cfg: &SandboxNetworkConfig, warnings: &mut Vec<PolicyWarning>) -> UnixSockets {
    if cfg.allow_all_unix_sockets {
        if !cfg.allow_unix_sockets.is_empty() {
            warnings.push(format!(
                "sandbox.network: allow_all_unix_sockets is set, so the {} \
                 enumerated allow_unix_sockets entries are ignored and every \
                 unix socket path is reachable",
                cfg.allow_unix_sockets.len()
            ));
        }
        return UnixSockets::All;
    }
    if cfg.allow_unix_sockets.is_empty() {
        return UnixSockets::Denied;
    }
    let (absolute, rejected): (Vec<String>, Vec<String>) = cfg
        .allow_unix_sockets
        .iter()
        .cloned()
        .partition(|p| p.starts_with('/'));
    for p in rejected {
        warnings.push(format!(
            "sandbox.network: ignoring allow_unix_sockets entry {p:?} because \
             the fence matches absolute paths only"
        ));
    }
    if absolute.is_empty() {
        UnixSockets::Denied
    } else {
        UnixSockets::Paths(absolute)
    }
}

#[cfg(test)]
#[path = "sandbox_policy_tests.rs"]
mod sandbox_policy_tests;
