//! Installing an approved authorization: directory capabilities widened on
//! the fence, rules and egress entries kept in the gate, and the rollback
//! that restores access before a partial install. Every function is
//! protocol-agnostic, taking the gate and sandbox session it reads.

use std::path::Path;
use std::sync::Arc;

use houyicoder_api::sandbox::{
    BoundaryAccess, BoundaryGrant, Containment, FenceAxis, SandboxSession, SideEffect,
    boundary_grants_for,
};
use houyicoder_context::SandboxError;
use houyicoder_permission::{ModeGate, Scope, StoreError};
use houyicoder_protocol::frontend::permission::AskReason;
use houyicoder_protocol::frontend::run::ApprovalScope;

use crate::composition::ContainmentAdapter;

use super::policy::{CardFence, DurableGrant, durable_grant, installed_scope};

/// The two fence facts an approval card's option list reads.
pub(crate) fn card_fence(gate: &dyn ModeGate) -> CardFence {
    CardFence {
        writes_confined: gate.fence_capability().confines(FenceAxis::Writes),
        egress_blocked: gate.fence_would_block(SideEffect::Network).is_some(),
    }
}

/// Analyze directory capabilities required by one approved target.
pub(crate) fn approval_directory_grants(
    sandbox: Option<&Arc<dyn SandboxSession>>,
    tool_name: &str,
    input: &serde_json::Value,
) -> Vec<BoundaryGrant> {
    let Some(session) = sandbox else {
        return Vec::new();
    };
    let bounds = ContainmentAdapter(session.clone());
    let Some(root) = bounds.boundary_root() else {
        return Vec::new();
    };
    boundary_grants_for(
        tool_name,
        Some(input),
        &root,
        &bounds.boundary_dirs(),
        &bounds.boundary_write_dirs(),
    )
}

/// Derive the durable authorization an approved answer on a call may keep.
pub(crate) fn consent_plan(
    gate: &dyn ModeGate,
    tool_name: &str,
    input: &serde_json::Value,
    reason: Option<&AskReason>,
    grants: &[BoundaryGrant],
) -> DurableGrant {
    durable_grant(tool_name, input, reason, card_fence(gate), grants)
}

/// Install the rule or destinations a grant plan keeps, at the lifetime the
/// caller already reduced to what the plan allows. A directory plan installs
/// nothing here: its capability is the fence. A save failure is returned so
/// the caller reports the session lifetime the rule still holds in memory.
pub(crate) fn apply_install(
    gate: &dyn ModeGate,
    grant: &DurableGrant,
    lifetime: ApprovalScope,
) -> Result<(), StoreError> {
    if lifetime == ApprovalScope::Once {
        return Ok(());
    }
    match grant {
        DurableGrant::Once | DurableGrant::Directories => Ok(()),
        DurableGrant::Network(destinations) => {
            for destination in destinations {
                gate.add_egress_allow(destination);
            }
            Ok(())
        }
        DurableGrant::Rule(rule) => {
            // The plan chose the store the rule belongs in; the client's
            // session choice keeps it in memory instead.
            let scope = match lifetime {
                ApprovalScope::Session => Scope::Session,
                _ => rule.scope,
            };
            match gate.add_rule(rule.clone().with_scope(scope)) {
                Ok(()) => Ok(()),
                Err(failure) => {
                    if let Err(error) = gate.add_rule(rule.clone().with_scope(Scope::Session)) {
                        tracing::warn!(
                            "an approved rule could not be kept even for this session: {error}"
                        );
                    }
                    Err(failure)
                }
            }
        }
    }
}

/// Install a derived plan at the lifetime it keeps. The fence install happens
/// at every lifetime because the frozen call has to run now; the lifetime
/// decides only what outlives it.
pub(crate) fn install_consent(
    sandbox: Option<&Arc<dyn SandboxSession>>,
    gate: &dyn ModeGate,
    grant: &DurableGrant,
    lifetime: ApprovalScope,
    grants: &[BoundaryGrant],
) -> Result<ConsentInstall, SandboxError> {
    let mut save_error = install_directory_grants(sandbox, gate, grants, lifetime)?;
    if let Err(error) = apply_install(gate, grant, lifetime) {
        save_error.get_or_insert(error);
    }
    Ok(ConsentInstall {
        lifetime: if save_error.is_some() {
            ApprovalScope::Session
        } else {
            lifetime
        },
        save_error,
    })
}

/// Route an approved call to its directory capability and chosen lifetime.
pub(crate) fn route_consent(
    sandbox: Option<&Arc<dyn SandboxSession>>,
    gate: &dyn ModeGate,
    tool_name: &str,
    input: &serde_json::Value,
    choice: ApprovalScope,
    reason: Option<&AskReason>,
) -> Result<ConsentInstall, SandboxError> {
    let grants = approval_directory_grants(sandbox, tool_name, input);
    route_consent_with_grants(sandbox, gate, tool_name, input, choice, reason, &grants)
}

/// Apply the exact directory grants disclosed on the approval card, then
/// install the authorization the chosen lifetime keeps. The lifetime is
/// reduced against the plan before anything is installed.
fn route_consent_with_grants(
    sandbox: Option<&Arc<dyn SandboxSession>>,
    gate: &dyn ModeGate,
    tool_name: &str,
    input: &serde_json::Value,
    choice: ApprovalScope,
    reason: Option<&AskReason>,
    grants: &[BoundaryGrant],
) -> Result<ConsentInstall, SandboxError> {
    let grant = consent_plan(gate, tool_name, input, reason, grants);
    let lifetime = installed_scope(&grant, choice);
    install_consent(sandbox, gate, &grant, lifetime, grants)
}

/// Install directory grants derived from a structured tool target, for exactly
/// the lifetime the caller names.
pub(crate) fn apply_consent_directory(
    sandbox: Option<&Arc<dyn SandboxSession>>,
    gate: &dyn ModeGate,
    tool_name: &str,
    input: &serde_json::Value,
    lifetime: ApprovalScope,
) -> Result<ConsentInstall, SandboxError> {
    let grants = approval_directory_grants(sandbox, tool_name, input);
    let save_error = install_directory_grants(sandbox, gate, &grants, lifetime)?;
    Ok(ConsentInstall {
        lifetime: if save_error.is_some() {
            ApprovalScope::Session
        } else {
            lifetime
        },
        save_error,
    })
}

/// Apply whatever a remembered verdict authorizes, bounded by the lifetime the
/// client chose. A compound command yields no rule, approved once only.
pub(crate) fn apply_consent_rule(
    gate: &dyn ModeGate,
    tool_name: &str,
    input: &serde_json::Value,
    reason: Option<&AskReason>,
    grants: &[BoundaryGrant],
    choice: ApprovalScope,
) -> Result<(), StoreError> {
    let grant = durable_grant(tool_name, input, reason, card_fence(gate), grants);
    apply_install(gate, &grant, installed_scope(&grant, choice))
}

/// Install runtime grants before persisting them. A partial update is rolled
/// back when a later grant fails, restoring the access each directory had
/// before. A save the store rejects does not undo the fence grant: the
/// approved call has to run now, so only the copy that would survive a
/// restart is missing.
pub(crate) fn install_directory_grants(
    sandbox: Option<&Arc<dyn SandboxSession>>,
    gate: &dyn ModeGate,
    grants: &[BoundaryGrant],
    lifetime: ApprovalScope,
) -> Result<Option<StoreError>, SandboxError> {
    if grants.is_empty() {
        return Ok(None);
    }
    let session = sandbox.ok_or_else(|| {
        SandboxError::SandboxUnavailable("no sandbox session for directory grant".into())
    })?;
    let fence = session.as_containment();
    let revision_before = fence.map(|f| f.capability_revision());
    // What each grant changed, so a rollback restores that state. A grant the
    // session already holds is left out: removing it would revoke an
    // authorization the user made earlier rather than undo this install.
    let mut installed: Vec<(&BoundaryGrant, PriorAccess)> = Vec::new();
    for grant in grants {
        if !grant.directory.is_dir() {
            rollback_directory_grants(session.as_ref(), &installed);
            return Err(SandboxError::NotFound(format!(
                "authorized directory does not exist: {}",
                grant.directory.display()
            )));
        }
        let Some(prior) = access_change(session.as_ref(), grant) else {
            continue;
        };
        let path = grant.directory.to_string_lossy();
        let result = match grant.access {
            BoundaryAccess::ReadOnly => session.add_reading_dir(&path),
            BoundaryAccess::ReadWrite => session.add_working_dir(&path),
        };
        if let Err(error) = result {
            rollback_directory_grants(session.as_ref(), &installed);
            return Err(error);
        }
        installed.push((grant, prior));
    }
    // The fence reports a revision that moves when its capability moves. An
    // install it accepted without moving that revision did not take effect.
    if !installed.is_empty()
        && let (Some(f), Some(before)) = (fence, revision_before)
        && f.capability_revision() <= before
    {
        rollback_directory_grants(session.as_ref(), &installed);
        return Err(SandboxError::Unsupported(
            "the fence accepted a directory grant without widening its capability".into(),
        ));
    }
    // Only the workspace lifetime has somewhere to be saved: the store path is
    // resolved per workspace, and the next start re-attaches what it finds
    // there. Remaining grants are still attempted after one save fails, so a
    // single unwritable record does not cost the others their grant.
    let mut save_error = None;
    if lifetime == ApprovalScope::Workspace {
        for grant in grants {
            let saved = match grant.access {
                BoundaryAccess::ReadOnly => gate.add_read_directory(&grant.directory, Scope::Local),
                BoundaryAccess::ReadWrite => gate.add_directory(&grant.directory, Scope::Local),
            };
            if let Err(error) = saved {
                save_error.get_or_insert(error);
            }
        }
    }
    Ok(save_error)
}

/// The outcome of installing an approved authorization: the lifetime the
/// grant actually keeps, plus why it keeps less than the card stated.
#[derive(Debug)]
pub(crate) struct ConsentInstall {
    lifetime: ApprovalScope,
    save_error: Option<StoreError>,
}

impl ConsentInstall {
    /// The lifetime the grant really has.
    pub(crate) fn lifetime(&self) -> ApprovalScope {
        self.lifetime
    }

    /// Why the grant keeps less than the card stated, when it does.
    pub(crate) fn save_error(&self) -> Option<&StoreError> {
        self.save_error.as_ref()
    }
}

/// What the session granted a directory before this install touched it, so a
/// rollback restores that state rather than revoking an earlier grant.
#[derive(Clone, Copy)]
enum PriorAccess {
    /// The directory was outside the fence.
    Absent,
    /// The directory was granted for reading only.
    ReadOnly,
}

/// The state to restore if this grant is rolled back, or none when the session
/// already grants the requested access.
fn access_change(session: &dyn SandboxSession, grant: &BoundaryGrant) -> Option<PriorAccess> {
    match grant.access {
        BoundaryAccess::ReadOnly => {
            if grant_already_held(&session.working_dirs(), &grant.directory) {
                None
            } else {
                Some(PriorAccess::Absent)
            }
        }
        BoundaryAccess::ReadWrite => {
            if grant_already_held(&session.writing_dirs(), &grant.directory) {
                None
            } else if grant_already_held(&session.reading_dirs(), &grant.directory) {
                Some(PriorAccess::ReadOnly)
            } else {
                Some(PriorAccess::Absent)
            }
        }
    }
}

/// Undo the grants this install added, most recent first: a directory that had
/// read access before keeps it, one outside the fence is removed again.
fn rollback_directory_grants(
    session: &dyn SandboxSession,
    installed: &[(&BoundaryGrant, PriorAccess)],
) {
    for (grant, prior) in installed.iter().rev() {
        let path = grant.directory.to_string_lossy();
        session.remove_working_dir(&path);
        if matches!(prior, PriorAccess::ReadOnly)
            && let Err(error) = session.add_reading_dir(&path)
        {
            tracing::warn!(
                "directory grant rollback could not restore read access to {}: {error}",
                grant.directory.display()
            );
        }
    }
}

/// Whether the session already holds this grant, canonicalized on both sides.
fn grant_already_held(held: &[String], directory: &Path) -> bool {
    let canonical = dunce::canonicalize(directory).ok();
    held.iter().any(|current| match &canonical {
        Some(target) => Path::new(current) == target,
        None => Path::new(current) == directory,
    })
}
