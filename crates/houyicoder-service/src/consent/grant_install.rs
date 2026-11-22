//! Installing an approved authorization: directory capabilities widened on
//! the fence, rules and egress entries kept in the gate, and the rollback
//! that restores access before a partial install. ConsentPolicy owns these
//! methods so every protocol endpoint installs the same grant.

use std::path::Path;

use houyicoder_api::sandbox::{
    BoundaryAccess, BoundaryGrant, Containment, FenceAxis, SandboxSession, SideEffect,
    boundary_grants_for,
};
use houyicoder_api::tool::is_shell_tool;
use houyicoder_context::SandboxError;
use houyicoder_permission::{Scope, StoreError};
use houyicoder_protocol::frontend::permission::AskReason;
use houyicoder_protocol::frontend::run::ApprovalScope;

use crate::composition::ContainmentAdapter;

use super::policy::{CardFence, DurableGrant, durable_grant, installed_scope};
use super::{ConsentPolicy, format_skill_script_detail};

impl ConsentPolicy {
    /// The two fence facts an approval card's option list reads.
    pub(crate) fn card_fence(&self) -> CardFence {
        CardFence {
            writes_confined: self.gate.fence_capability().confines(FenceAxis::Writes),
            egress_blocked: self.gate.fence_would_block(SideEffect::Network).is_some(),
        }
    }

    /// Analyze directory capabilities required by one approved target.
    pub(crate) fn approval_directory_grants(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Vec<BoundaryGrant> {
        let Some(session) = self.sandbox_session.as_ref() else {
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
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
    ) -> DurableGrant {
        durable_grant(tool_name, input, reason, self.card_fence(), grants)
    }

    /// Install the rule or destinations a grant plan keeps. A directory plan
    /// installs nothing here. A save failure is returned so the caller reports
    /// the session lifetime the rule still holds in memory.
    pub(crate) fn apply_install(
        &self,
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
                    self.gate.add_egress_allow(destination);
                }
                Ok(())
            }
            DurableGrant::Rule(rule) => {
                let scope = match lifetime {
                    ApprovalScope::Session => Scope::Session,
                    _ => rule.scope,
                };
                match self.gate.add_rule(rule.clone().with_scope(scope)) {
                    Ok(()) => Ok(()),
                    Err(failure) => {
                        if let Err(error) =
                            self.gate.add_rule(rule.clone().with_scope(Scope::Session))
                        {
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

    /// Install a derived plan at the lifetime it keeps. The fence install
    /// happens at every lifetime because the frozen call has to run now; the
    /// lifetime decides only what outlives it.
    pub(crate) fn install_consent(
        &self,
        grant: &DurableGrant,
        lifetime: ApprovalScope,
        grants: &[BoundaryGrant],
    ) -> Result<ConsentInstall, SandboxError> {
        let mut save_error = self.install_directory_grants(grants, lifetime)?;
        if let Err(error) = self.apply_install(grant, lifetime) {
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
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        choice: ApprovalScope,
        reason: Option<&AskReason>,
    ) -> Result<ConsentInstall, SandboxError> {
        let grants = self.approval_directory_grants(tool_name, input);
        self.route_consent_with_grants(tool_name, input, choice, reason, &grants)
    }

    /// Apply the exact directory grants disclosed on the card, then install
    /// the authorization the chosen lifetime keeps. The lifetime is reduced
    /// against the plan before anything is installed.
    fn route_consent_with_grants(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        choice: ApprovalScope,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
    ) -> Result<ConsentInstall, SandboxError> {
        let grant = self.consent_plan(tool_name, input, reason, grants);
        let lifetime = installed_scope(&grant, choice);
        self.install_consent(&grant, lifetime, grants)
    }

    /// Install directory grants derived from a structured tool target, for
    /// exactly the lifetime the caller names.
    pub(crate) fn apply_consent_directory(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        lifetime: ApprovalScope,
    ) -> Result<ConsentInstall, SandboxError> {
        let grants = self.approval_directory_grants(tool_name, input);
        let save_error = self.install_directory_grants(&grants, lifetime)?;
        Ok(ConsentInstall {
            lifetime: if save_error.is_some() {
                ApprovalScope::Session
            } else {
                lifetime
            },
            save_error,
        })
    }

    /// Apply whatever a remembered verdict authorizes, bounded by the lifetime
    /// the client chose. A compound command yields no rule, approved once.
    pub(crate) fn apply_consent_rule(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        reason: Option<&AskReason>,
        grants: &[BoundaryGrant],
        choice: ApprovalScope,
    ) -> Result<(), StoreError> {
        let grant = durable_grant(tool_name, input, reason, self.card_fence(), grants);
        self.apply_install(&grant, installed_scope(&grant, choice))
    }

    /// Replace the protected-path detail with the skill script path when a
    /// Bash command runs a skill-directory script.
    pub(crate) fn augment_skill_script_reason(
        &self,
        reason: &mut AskReason,
        tool_name: &str,
        input: &serde_json::Value,
    ) {
        if !is_shell_tool(tool_name) {
            return;
        }
        let Some(registry) = self.skill_registry.as_ref() else {
            return;
        };
        let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
            return;
        };
        let scripts = registry.detect_run_scripts(command);
        if scripts.is_empty() {
            return;
        }
        reason.detail = format_skill_script_detail(&scripts);
    }

    /// Install runtime grants before persisting them. A partial update is
    /// rolled back when a later grant fails. A save the store rejects does not
    /// undo the fence grant: the approved call has to run now, so only the
    /// copy that would survive a restart is missing.
    pub(crate) fn install_directory_grants(
        &self,
        grants: &[BoundaryGrant],
        lifetime: ApprovalScope,
    ) -> Result<Option<StoreError>, SandboxError> {
        if grants.is_empty() {
            return Ok(None);
        }
        let session = self.sandbox_session.as_ref().ok_or_else(|| {
            SandboxError::SandboxUnavailable("no sandbox session for directory grant".into())
        })?;
        let fence = session.as_containment();
        let revision_before = fence.map(|f| f.capability_revision());
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
        if !installed.is_empty()
            && let (Some(f), Some(before)) = (fence, revision_before)
            && f.capability_revision() <= before
        {
            rollback_directory_grants(session.as_ref(), &installed);
            return Err(SandboxError::Unsupported(
                "the fence accepted a directory grant without widening its capability".into(),
            ));
        }
        let mut save_error = None;
        if lifetime == ApprovalScope::Workspace {
            for grant in grants {
                let saved = match grant.access {
                    BoundaryAccess::ReadOnly => {
                        self.gate.add_read_directory(&grant.directory, Scope::Local)
                    }
                    BoundaryAccess::ReadWrite => {
                        self.gate.add_directory(&grant.directory, Scope::Local)
                    }
                };
                if let Err(error) = saved {
                    save_error.get_or_insert(error);
                }
            }
        }
        Ok(save_error)
    }
}

/// The outcome of installing an approved authorization.
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

/// What the session granted a directory before this install touched it.
#[derive(Clone, Copy)]
enum PriorAccess {
    Absent,
    ReadOnly,
}

/// The state to restore if this grant is rolled back, or none when already held.
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

/// Undo the grants this install added, most recent first.
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
