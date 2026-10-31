//! The outbound direction of the connection driver: the frame a client
//! command becomes. Every command answers with exactly one frame, so the
//! driver only has to queue it.

use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{ClientResponsePayload, RequestId};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::trust::TrustAccept;

use crate::agent_message::ClientCommand;

use super::Outbound;
use super::{
    cancel_child_turn_notification, cancel_notification, focus_notification,
    inject_child_notification, inject_notification, kill_all_notification, kill_child_notification,
    queue_remove_notification,
};

fn request(req_id: RequestId, payload: FrontendRequest) -> Outbound {
    Outbound::Request { req_id, payload }
}

fn reverse(req_id: RequestId, payload: ClientResponsePayload) -> Outbound {
    Outbound::Reverse { req_id, payload }
}

fn notice_frame(notice: AcpNotification) -> Outbound {
    Outbound::Notification(notice)
}

/// The frame one client command asks the driver to send.
///
/// The table covers every command in one exhaustive match, which a reader can
/// check against the command enum in one pass; splitting it would repeat the
/// variant set in each piece and move the exhaustiveness check away from the
/// place a new command has to be answered.
#[expect(
    clippy::too_many_lines,
    reason = "one exhaustive branch per client command; a split repeats the variant set per piece"
)]
pub(super) fn frame_for(command: ClientCommand) -> Outbound {
    match command {
        ClientCommand::SendMessage {
            req_id,
            session_id,
            content,
            disabled_skills,
        } => request(
            req_id,
            FrontendRequest::MessageSend {
                session_id,
                content,
                disabled_skills,
            },
        ),
        ClientCommand::Verdict { req_id, decision } => {
            reverse(req_id, ClientResponsePayload::Permission(decision))
        }
        ClientCommand::TrustVerdict { req_id, accept } => reverse(
            req_id,
            ClientResponsePayload::TrustAccept(TrustAccept { accepted: accept }),
        ),
        ClientCommand::StatusQuery { req_id } => request(req_id, FrontendRequest::Status),
        ClientCommand::TrajectoryQuery { req_id } => request(req_id, FrontendRequest::Trajectory),
        ClientCommand::ContextQuery { req_id } => request(req_id, FrontendRequest::Context),
        ClientCommand::CompactQuery { req_id } => request(req_id, FrontendRequest::Compact),
        ClientCommand::PermissionModeQuery { req_id } => {
            request(req_id, FrontendRequest::PermissionMode)
        }
        ClientCommand::PermissionRulesQuery { req_id } => {
            request(req_id, FrontendRequest::PermissionRules)
        }
        ClientCommand::ToolListQuery { req_id } => request(req_id, FrontendRequest::ToolList),
        ClientCommand::AgentsQuery { req_id } => request(req_id, FrontendRequest::Agents),
        ClientCommand::ChildTranscriptQuery { req_id, child_sid } => {
            request(req_id, FrontendRequest::ChildTranscript { child_sid })
        }
        ClientCommand::HooksQuery { req_id } => request(req_id, FrontendRequest::Hooks),
        ClientCommand::SkillBodyQuery { req_id, name } => {
            request(req_id, FrontendRequest::SkillBody { name })
        }
        ClientCommand::SkillsQuery { req_id } => request(req_id, FrontendRequest::Skills),
        ClientCommand::MemoryListQuery { req_id } => request(req_id, FrontendRequest::MemoryList),
        ClientCommand::MemoryShowQuery { req_id, key } => {
            request(req_id, FrontendRequest::MemoryShow { key })
        }
        ClientCommand::MemoryToggleStateQuery { req_id } => {
            request(req_id, FrontendRequest::MemoryToggleState)
        }
        ClientCommand::MemoryToggleQuery { req_id, which } => {
            request(req_id, FrontendRequest::MemoryToggle { which })
        }
        ClientCommand::RecapConfigSet {
            req_id,
            enabled,
            idle_secs,
        } => request(
            req_id,
            FrontendRequest::RecapConfigSet { enabled, idle_secs },
        ),
        ClientCommand::MemoryForgetQuery { req_id, key, scope } => {
            request(req_id, FrontendRequest::MemoryForget { key, scope })
        }
        ClientCommand::ModelInfoQuery { req_id } => request(req_id, FrontendRequest::ModelInfo),
        ClientCommand::SandboxBoundariesQuery { req_id } => {
            request(req_id, FrontendRequest::SandboxBoundaries)
        }
        ClientCommand::SandboxCheckpointsQuery { req_id } => {
            request(req_id, FrontendRequest::SandboxCheckpoints)
        }
        ClientCommand::SandboxRestoreQuery { req_id, operation } => {
            request(req_id, FrontendRequest::SandboxRestore { operation })
        }
        ClientCommand::SandboxTextsQuery { req_id, operation } => {
            request(req_id, FrontendRequest::SandboxTexts { operation })
        }
        ClientCommand::WorktreePreviewQuery { req_id, path } => {
            request(req_id, FrontendRequest::WorktreePreview { path })
        }
        ClientCommand::WorktreeEnterQuery { req_id, path } => {
            request(req_id, FrontendRequest::WorktreeEnter { path })
        }
        ClientCommand::WorktreeRemoveQuery {
            req_id,
            path,
            token,
            delete_unmerged_branch,
        } => request(
            req_id,
            FrontendRequest::WorktreeRemove {
                path,
                token,
                delete_unmerged_branch,
            },
        ),
        ClientCommand::ModelSwitch {
            req_id,
            model,
            effort,
            effort_toggled,
            speed,
        } => request(
            req_id,
            FrontendRequest::ModelSet {
                model,
                effort,
                effort_toggled,
                speed,
            },
        ),
        ClientCommand::RenameSessionQuery {
            req_id,
            session_id,
            name,
        } => request(req_id, FrontendRequest::RenameSession { session_id, name }),
        ClientCommand::PermissionCycleModeQuery { req_id } => {
            request(req_id, FrontendRequest::PermissionCycleMode)
        }
        ClientCommand::PermissionAddRuleQuery { req_id, rule } => {
            request(req_id, FrontendRequest::PermissionAddRule { rule })
        }
        ClientCommand::PermissionRemoveRuleQuery { req_id, index } => {
            request(req_id, FrontendRequest::PermissionRemoveRule { index })
        }
        ClientCommand::PermissionAddDirQuery { req_id, path } => {
            request(req_id, FrontendRequest::PermissionAddWorkingDir { path })
        }
        ClientCommand::PermissionRemoveDirQuery { req_id, path } => {
            request(req_id, FrontendRequest::PermissionRemoveWorkingDir { path })
        }
        ClientCommand::PermissionAskBeforeGitQuery { req_id, enabled } => {
            request(req_id, FrontendRequest::PermissionAskBeforeGit { enabled })
        }
        // The session aborts the run token; no id, so the outcome returns on
        // the original run's request.
        ClientCommand::AbortRun { session_id } => notice_frame(cancel_notification(&session_id)),
        // Mid-turn input enters the queue under the id the terminal sent, so
        // the transcript shows it once the drive loop drains it.
        ClientCommand::InjectUser { session_id, input } => {
            notice_frame(inject_notification(&session_id, &input))
        }
        // Text reaches a running child's inbox without starting a parent turn.
        ClientCommand::InjectToChild { child_sid, text } => {
            notice_frame(inject_child_notification(&child_sid, &text))
        }
        // The viewed child drops its current fetch and starts the next turn.
        ClientCommand::CancelChildTurn { child_sid } => {
            notice_frame(cancel_child_turn_notification(&child_sid))
        }
        // The child's lifecycle ends; its completion drops the strip row.
        ClientCommand::KillChild { child_sid } => notice_frame(kill_child_notification(&child_sid)),
        ClientCommand::KillAllChildren => notice_frame(kill_all_notification()),
        ClientCommand::ReportFocus { focused } => notice_frame(focus_notification(focused)),
        // Only the exact identified queue item leaves the queue.
        ClientCommand::QueueRemove { session_id, id } => {
            notice_frame(queue_remove_notification(&session_id, id))
        }
        ClientCommand::SessionReset { req_id, session_id } => {
            request(req_id, FrontendRequest::SessionReset { session_id })
        }
        ClientCommand::DebugSet { req_id, level } => {
            request(req_id, FrontendRequest::DebugSet { level })
        }
    }
}
