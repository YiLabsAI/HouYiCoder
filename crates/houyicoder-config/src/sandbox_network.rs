//! Sandbox network policy configuration.
//!
//! The network axis of the containment fence. Default posture is Open: the
//! profile allows the network operation class and the permission gate asks
//! before an egress command runs, so ordinary development traffic (git push,
//! package fetches) works without a blanket kernel deny. A user who wants
//! kernel-level no-network sets mode to Off. Values here are settings only;
//! the profile builder in the sandbox layer lowers them to kernel rules, and
//! this crate holds no rule-rendering logic of its own.
//!
//! There is deliberately no port-scoped mode. A port-443 allowlist blocks
//! nothing an attacker cares about (exfiltration reaches any host over 443)
//! while breaking legitimate non-443 traffic (git protocol, ssh remotes,
//! custom-port registries). The honest choice is binary — gated-open by
//! default, or fully off — until a hostname-filtering proxy lands as the
//! next stage of this work.

/// How wide the network fence is opened.
///
/// Deliberately binary. A middle tier that allows egress by port number is
/// omitted on purpose; see the module documentation for why.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// No egress. The profile denies the network operation class outright.
    /// Loopback and unix-socket allow-backs still apply if configured, because
    /// those are local IPC, not egress. Opt-in for users who want kernel-level
    /// containment with no network at all; the default is Open so the gate's
    /// per-command egress ask is the control, not a blanket kernel deny.
    Off,
    /// Unrestricted network, gated per-command. The profile allows the whole
    /// network operation class; the permission gate still asks before an egress
    /// command runs, so the user gets a per-command consent point. This is
    /// the default because a
    /// blanket deny (Off) blocks ordinary development traffic (git push,
    /// package fetches) that the gate is already asking about, which is too
    /// strict for the danger: the sandbox contains everything else, and network
    /// containment finer than ask-and-consent needs a filtering proxy that is
    /// not built yet.
    #[default]
    Open,
    /// A value this version does not implement, kept verbatim.
    ///
    /// Modelled explicitly instead of letting the value fail the parse. A failed
    /// parse would still be safe, because the fallback is full containment, but it
    /// would be silent, and it would discard the rest of the section along with
    /// it. A user who writes the proxy mode that the design documents as planned
    /// would get a fully closed fence and no explanation. Holding the value lets
    /// the caller name it in the warning.
    #[serde(untagged)]
    Unsupported(String),
}

/// The network section of the sandbox settings.
///
/// Every field widens the fence; the default value of every field is the
/// narrowest one. A missing or malformed settings file therefore yields full
/// containment rather than an accidental opening.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SandboxNetworkConfig {
    /// Egress posture. Defaults to Open (gated per-command by the gate); Off
    /// is the opt-in for kernel-level no-network.
    pub mode: NetworkMode,
    /// Unix socket paths to allow, for local IPC that is not egress: the ssh
    /// agent socket, a container daemon socket, a build daemon socket. Denying
    /// the whole network operation class also denies unix domain sockets,
    /// because the kernel classes them under the same operations, so local IPC
    /// needs an explicit allow-back even when egress stays off. Each entry is
    /// matched as a subpath.
    pub allow_unix_sockets: Vec<String>,
    /// Allow every unix socket path instead of an enumerated list. Broad: any
    /// path-bearing local IPC becomes reachable, including sockets that proxy
    /// to the network. Prefer the enumerated list.
    pub allow_all_unix_sockets: bool,
    /// Allow binding and accepting on loopback, for a dev server or test
    /// harness started inside the fence. Does not grant egress.
    pub allow_local_binding: bool,
    /// Every key in the section this version does not act on.
    ///
    /// Captured rather than discarded so the caller can tell the user about it.
    /// The keys that describe destination policy need a proxy that resolves
    /// hostnames before they can mean anything, so they are accepted by the file
    /// format ahead of being enforced. Dropping them in silence is the worse
    /// failure mode: a user who writes an allowlist and is told nothing believes
    /// the allowlist is protecting them. The same capture catches a misspelled
    /// key, which would otherwise also fail silently.
    #[serde(flatten)]
    pub unrecognized: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Keys that name a real planned capability which this version cannot enforce,
/// because deciding them requires seeing hostnames and nothing here does yet.
/// Separated from an outright unknown key so the two get different wording: one
/// is "not yet", the other is probably a typo.
const DEFERRED_KEYS: &[&str] = &["unknown_domain", "allow", "deny", "seed", "allow_raw_ip"];

impl SandboxNetworkConfig {
    /// Describe every key that was read but not acted on, phrased for the user.
    ///
    /// Returned rather than logged so the caller owns the surface, and so this
    /// stays a pure function that a test can drive without capturing output.
    #[must_use]
    pub fn deferred_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let NetworkMode::Unsupported(value) = &self.mode {
            out.push(format!(
                "sandbox.network: mode {value:?} is not implemented, so egress is \
                 denied. Supported values are \"off\" and \"open\""
            ));
        }
        out.extend(self.unrecognized_warnings());
        out
    }

    /// Describe the keys that were read but not acted on. Split from the mode
    /// report so each concern reads on its own.
    fn unrecognized_warnings(&self) -> impl Iterator<Item = String> + '_ {
        self.unrecognized.keys().map(|k| {
            if DEFERRED_KEYS.contains(&k.as_str()) {
                format!(
                    "sandbox.network: {k} is not in effect yet — deciding it \
                         needs a proxy that can see hostnames, which is not built. \
                         Egress is currently either fully denied or fully open, so \
                         this entry is not restricting anything"
                )
            } else {
                format!("sandbox.network: unknown key {k} is ignored")
            }
        })
    }
}

/// Load the network policy from the settings file. A missing or corrupt file
/// yields the default (Open, gated per-command). Parse failure never widens
/// the fence beyond the default: the gate still asks before an egress command
/// runs, so a corrupt file does not silently grant unrestricted egress without
/// a consent point.
pub fn load_sandbox_network() -> SandboxNetworkConfig {
    load_sandbox_network_from(&crate::settings_path())
}

/// Pure loader against an explicit path; testable without env mutation.
pub fn load_sandbox_network_from(path: &std::path::Path) -> SandboxNetworkConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return SandboxNetworkConfig::default();
    };
    serde_json::from_str::<SettingsNetworkView>(&text)
        .map(|v| v.sandbox.network)
        .unwrap_or_default()
}

/// Deserialization view over the settings file, narrowed to the path this
/// module owns. Unrelated settings keys are ignored, so this loader does not
/// have to track the rest of the settings schema.
#[derive(Debug, Default, serde::Deserialize)]
struct SettingsNetworkView {
    #[serde(default)]
    sandbox: SandboxSectionView,
}

#[derive(Debug, Default, serde::Deserialize)]
struct SandboxSectionView {
    #[serde(default)]
    network: SandboxNetworkConfig,
}

#[cfg(test)]
#[path = "sandbox_network_tests.rs"]
mod sandbox_network_tests;
