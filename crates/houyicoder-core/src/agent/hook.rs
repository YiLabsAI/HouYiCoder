//! Hook system: pure-verdict event subscription with host-executed flow
//! control. A hook returns a verdict value and the host decides what to do
//! with it, so dispatch stays replayable and a hook cannot take over the
//! flow. All flow control is host-side: a deny gate stops a security-sensitive
//! event, a feedback signal joins the next model prompt with bounded retry,
//! and observe-transform only watches or injects. Structured errors replace
//! exit-code-plus-stderr signaling, so a failing hook never crashes the host.

#![allow(dead_code)] // stub hook types and reserved traits pending owner-crate wiring; locally unused

pub(crate) mod command;
mod event;
pub(crate) mod filter;
pub(crate) mod fire;
pub(crate) mod metadata;
pub(crate) mod pipeline;
pub(crate) mod registry;
mod verdict;
pub(crate) mod wire;

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

use houyicoder_api::trust::TrustState;

pub use event::{CompactTrigger, HookEvent, HookPayload, SessionEndReason, ToolResult};
pub(crate) use registry::HookRegistry;
pub use verdict::{CombinedVerdict, HookContext, HookError, HookVerdict, combine_verdicts};
pub use wire::HookOutcome;

/// Capability token scoping what host functions a hook guest may call.
/// Deny-by-default at the ABI boundary: a guest sees only declared
/// capabilities. Opaque so guest code cannot forge one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityToken {
    capabilities: Vec<String>,
}

impl CapabilityToken {
    pub fn new(caps: Vec<String>) -> Self {
        Self { capabilities: caps }
    }

    pub fn has(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }
}

/// Policy gating resolved at config-load time (not per-dispatch). Controls
/// which hook sources are active.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HookPolicy {
    /// All sources (user, project, local, managed) are active.
    #[default]
    AllEnabled,
    /// Only managed/policy hooks run.
    ManagedOnly,
    /// Only plugin-registered hooks run.
    PluginOnly,
    /// All hooks disabled.
    Disabled,
    /// Non-managed hooks disabled (user, project, local); managed hooks
    /// still run. This mirrors a managed setting that turns off all
    /// user-configured hooks while keeping policy-deployed hooks active.
    NonManagedDisabled,
}

/// Which configuration level a hook was registered from. The registry
/// uses this to filter hooks by policy at dispatch time. Four levels track
/// the configuration merge hierarchy: user (global config on the user's
/// machine), project (checked into a repository), local (gitignored
/// overrides), and managed (policy-pushed, for example via MDM).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSource {
    User,
    Project,
    Local,
    Managed,
}

// Hook trait — pure-verdict subscription.

/// Pure-verdict hook trait. A hook subscribes to events and returns a
/// verdict; it never executes flow control itself.
///
/// Execution model: WASM in-process (hard-isolated, default) or
/// external-process. The host wraps external-process hooks with an
/// async-to-sync bridge so the trait stays synchronous. Verdicts should be
/// fast (mechanical rules on the hot path, no model calls) and deterministic
/// for replayability.
pub trait Hook: Send + Sync {
    /// Human-readable hook name for diagnostics. Carried into HookError
    /// variants (GuestPanic, Timeout, InvalidVerdict) so the user can see
    /// which hook failed.
    fn name(&self) -> &str;

    /// Which events this hook subscribes to. The registry indexes by these.
    fn events(&self) -> &[HookEvent];

    /// Evaluate the event and payload, return a pure verdict. Must be
    /// deterministic: the same event and payload yield the same verdict.
    /// Errors are structured; never panic.
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError>;

    /// Which configuration level this hook was registered from. The
    /// registry uses this to filter hooks by policy at dispatch time.
    fn source(&self) -> HookSource;

    /// Per-hook timeout. None means the registry default applies. The
    /// registry uses this to set a per-hook deadline during dispatch so
    /// a slow hook does not block the whole dispatch budget.
    fn timeout(&self) -> Option<Duration> {
        None
    }

    /// Whether this hook fires only once. When true, the hook
    /// self-unregisters after the first attempt that actually produced
    /// a verdict. An attempt that failed (HookError) does not consume
    /// the one shot: the hook stays registered so a transient start
    /// failure can retry on the next event, since a hook the user asked
    /// to run once has not yet run. Concurrency is gated by a
    /// compare_exchange taken before the attempt, so at most one
    /// dispatch runs the hook at a time and the losers return Allow
    /// without running it; a failed attempt releases that gate again.
    fn once(&self) -> bool {
        false
    }
}

// HookExecutor — future WASM sandbox seam.

/// Executor for hook logic running inside a WASM sandbox. The host calls
/// the executor, which runs the guest component and translates traps into
/// structured HookError values.
///
/// TODO: implement via Wasmtime Component Model. The target shape is two
/// executors (in-process WASM plus external-process) to drop the SSRF
/// attack surface and keep model-classification off the hot path.
pub trait HookExecutor: Send + Sync {
    /// Run the hook guest for the given context, return its verdict.
    fn execute(&self, ctx: &HookContext) -> Result<HookVerdict, HookError>;
}

// policy_allows — source vs policy filter predicate.

/// Whether a hook source passes the active policy. ManagedOnly keeps only
/// Managed-source hooks. PluginOnly keeps only Project-source hooks — this
/// is a simplification: the plugin system will introduce a dedicated
/// Plugin source variant, but until it lands, Project is the closest
/// analog (plugins ship with the project). NonManagedDisabled blocks
/// User, Project, and Local but keeps Managed (a managed setting that
/// turns off user-configured hooks while keeping policy-deployed ones).
/// AllEnabled passes everything; Disabled blocks everything (but dispatch
/// short-circuits before reaching here).
fn policy_allows(source: &HookSource, policy: &HookPolicy) -> bool {
    match policy {
        HookPolicy::AllEnabled => true,
        HookPolicy::Disabled => false,
        HookPolicy::ManagedOnly => *source == HookSource::Managed,
        HookPolicy::PluginOnly => *source == HookSource::Project,
        HookPolicy::NonManagedDisabled => *source == HookSource::Managed,
    }
}

/// Whether a hook source passes the trust gate. For an Untrusted project,
/// only User and Managed sources pass: User lives in the user's home
/// directory; Managed is policy-pushed (e.g. via MDM). Project and Local
/// sources live in the repository directory — Local is a gitignored
/// override file inside the project, NOT on the user's machine — so a
/// malicious repo could ship a Local hook and run it without trust on an
/// untrusted project. The blanket trust gate: in interactive mode, ALL
/// hooks require trust before they execute.
/// Trusted and Acknowledged projects pass all sources.
fn trust_allows(source: &HookSource, trust: &TrustState) -> bool {
    match source {
        HookSource::User | HookSource::Managed => true,
        HookSource::Project | HookSource::Local => *trust != TrustState::Untrusted,
    }
}

// catch_panic — isolation guard for hook evaluation.

/// Wrap hook evaluation in a panic catch so a panicking hook never crashes
/// the host. A panic is converted to HookError::GuestPanic; the hook_name
/// comes from Hook::name(). True isolation (WASM guest trap) lands with the
/// sandbox executor.
fn catch_panic<F>(f: F, hook_name: &str) -> Result<HookVerdict, HookError>
where
    F: FnOnce() -> Result<HookVerdict, HookError>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let backtrace = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "non-string panic payload".to_string()
            };
            Err(HookError::GuestPanic {
                hook_name: hook_name.to_string(),
                backtrace,
            })
        }
    }
}

#[cfg(test)]
mod tests;
