//! The approval exchange over the ACP wire: the options the agent offers,
//! the reverse request that carries an ask to the client, and the reply the
//! client sends back, mapped to the engine decision resume consumes.

use houyicoder_context::PermissionVerdict;
use houyicoder_core::agent::ApprovalDecision as EngineApprovalDecision;
use houyicoder_core::agent::ApprovalRequest as EngineApprovalRequest;
use houyicoder_protocol::acp_wire::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use houyicoder_protocol::frontend::session_update::{ToolCallUpdate, ToolCallUpdateFields};

/// The four verdict options the agent offers on every permission ask. The
/// option ids are stable strings so the server maps a selected id back to an
/// approved/rejected verdict without stashing the offered list per ask. The
/// names track the wire wording so a stock client renders them verbatim. No
/// option states a lifetime: this set is built without the ask's plan, and a
/// client that picks the remember option leaves the lifetime to the server,
/// which keeps it no longer than the plan allows.
fn standard_permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: "allow_once".into(),
            name: "Allow once".into(),
            kind: PermissionOptionKind::AllowOnce,
            scope: None,
            meta: None,
        },
        PermissionOption {
            option_id: "allow_always".into(),
            name: "Always allow".into(),
            kind: PermissionOptionKind::AllowAlways,
            scope: None,
            meta: None,
        },
        PermissionOption {
            option_id: "reject_once".into(),
            name: "Reject once".into(),
            kind: PermissionOptionKind::RejectOnce,
            scope: None,
            meta: None,
        },
        PermissionOption {
            option_id: "reject_always".into(),
            name: "Always reject".into(),
            kind: PermissionOptionKind::RejectAlways,
            scope: None,
            meta: None,
        },
    ]
}

/// Map an engine approval request to the ACP reverse-request shape the agent
/// sends to the client mid-turn. The tool call under review rides a
/// ToolCallUpdate (call id plus raw input); the options are the four standard
/// verdicts. session_id is the display string of the session the ask is for.
pub(crate) fn approval_to_acp_permission(
    req: &EngineApprovalRequest,
    session_id: String,
) -> RequestPermissionRequest {
    let tool_call = ToolCallUpdate {
        tool_call_id: req.call_id.clone().into(),
        fields: ToolCallUpdateFields {
            raw_input: Some(req.input.clone()),
            ..Default::default()
        },
    };
    RequestPermissionRequest {
        session_id,
        tool_call,
        options: standard_permission_options(),
        meta: None,
    }
}

/// The engine decision an ACP permission response produces, with the verdict
/// the durable audit records for it.
pub(crate) struct AcpPermissionReply {
    /// What the resume path consumes: whether the call may run.
    pub(crate) decision: EngineApprovalDecision,
    /// The user's answer as the audit records it. A cancelled response holds
    /// none: the run was abandoned, not answered, so the card stays an
    /// outcome nobody stated.
    pub(crate) verdict: Option<PermissionVerdict>,
}

/// Map the client's permission response back to the engine decision the
/// resume path consumes. Selected maps the option id to approved/rejected
/// (allow options approve; reject options deny), and that answer is the
/// verdict the audit holds. Cancelled is a reap (the run was cancelled, not
/// answered) — the tool still sees a veto so the turn can end, but no verdict
/// is recorded, so a cancelled card counts as an unknown outcome rather than
/// a denial. updated_input is None in the first cut: the ACP outcome carries
/// a verdict only, not an edited input (the answer-populated input path is a
/// frontend-dialect feature that lands with acpx/elicitation).
pub(crate) fn acp_permission_response_to_decision(
    resp: RequestPermissionResponse,
    call_id: String,
) -> AcpPermissionReply {
    let verdict = match resp.outcome {
        RequestPermissionOutcome::Cancelled => None,
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => Some(
            if matches!(option_id.as_str(), "allow_once" | "allow_always") {
                PermissionVerdict::Approved
            } else {
                PermissionVerdict::Denied
            },
        ),
    };
    AcpPermissionReply {
        decision: EngineApprovalDecision {
            call_id,
            approved: verdict == Some(PermissionVerdict::Approved),
            updated_input: None,
        },
        verdict,
    }
}
