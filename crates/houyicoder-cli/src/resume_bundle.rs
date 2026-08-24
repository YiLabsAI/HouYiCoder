//! The resume + fork bundle builders: wrap the composition root's
//! build_runner_for_resume_sid / build_runner_for_fork in the TUI bundle the
//! run loop drives. Extracted from main.rs on size grounds; both route
//! through assemble_bundle (crate root) so the bridge wiring cannot drift from
//! the fresh path.

use std::sync::Arc;

/// Resume a session already on disk (--resume <sid>). Re-opens the existing
/// log + sidecar; the engine reads the history via backend replay on the
/// next run. Wires the same TUI bundle the fresh path uses, with the model
/// restored from the sidecar.
pub(super) fn build_bundle_for_resume_sid(
    sid: houyicoder_context::SessionId,
    project: Option<String>,
    provider: houyicoder_service::composition::ResolvedProvider,
) -> Result<houyicoder_tui::composition::RunnerBundle, Box<dyn std::error::Error>> {
    let resumed = houyicoder_service::composition::build_runner_for_resume_sid(
        sid,
        &houyicoder_service::composition::session_log_root(),
        project,
        Some(Arc::new(
            houyicoder_permission::FileRuleStore::default_paths(),
        )),
        provider,
    )?;
    Ok(crate::assemble_bundle(
        resumed.assembled.runner,
        resumed.assembled.session,
        resumed.model,
        resumed.assembled.gate,
        resumed.assembled.sandbox_session,
        resumed.assembled.append_notify,
        true,
        resumed.assembled.worktree_controller,
    ))
}

/// Fork an existing session (--resume <sid> --fork-session / --continue
/// --fork-session). Mints a new sid seeded from the source's durable events
/// with ForkedFrom provenance; the source is untouched. skip_login lands on
/// the working screen directly.
pub(super) fn build_bundle_for_fork(
    source_sid: houyicoder_context::SessionId,
    project: Option<String>,
    provider: houyicoder_service::composition::ResolvedProvider,
) -> Result<houyicoder_tui::composition::RunnerBundle, Box<dyn std::error::Error>> {
    let resumed = houyicoder_service::composition::build_runner_for_fork(
        source_sid,
        &houyicoder_service::composition::session_log_root(),
        project,
        Some(Arc::new(
            houyicoder_permission::FileRuleStore::default_paths(),
        )),
        provider,
    )?;
    Ok(crate::assemble_bundle(
        resumed.assembled.runner,
        resumed.assembled.session,
        resumed.model,
        resumed.assembled.gate,
        resumed.assembled.sandbox_session,
        resumed.assembled.append_notify,
        true,
        resumed.assembled.worktree_controller,
    ))
}
