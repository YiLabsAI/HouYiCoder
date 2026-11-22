//! The approval exchange over the ACP wire: build the reverse request that
//! carries an ask to the client, and map the reply back to the choice the
//! consent policy settles.

use houyicoder_core::agent::ApprovalRequest as EngineApprovalRequest;
use houyicoder_protocol::acp_wire::{
    PermissionOption, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use houyicoder_protocol::frontend::permission::AskReason;
use houyicoder_protocol::frontend::run::{ApprovalScope, DelegationSource};
use houyicoder_protocol::frontend::session_update::{ToolCallUpdate, ToolCallUpdateFields};
use serde_json::Map;

use crate::consent::{PlannedAsk, RawConsentChoice};

use super::{tool_kind, tool_locations, tool_title};

/// Map an engine approval request and its consent plan to the ACP
/// reverse-request shape. The tool call under review rides a ToolCallUpdate
/// (title, kind, locations, raw input). The options are the ones the plan
/// derived, so a remember choice is offered only when the grant can install.
/// The plan's disclosed reason and any delegation travel in _meta.
pub(crate) fn approval_to_acp_permission(
    req: &EngineApprovalRequest,
    session_id: String,
    options: &[PermissionOption],
    reason: Option<&AskReason>,
    delegation: Option<&DelegationSource>,
) -> RequestPermissionRequest {
    let tool_call = ToolCallUpdate {
        tool_call_id: req.call_id.clone().into(),
        fields: ToolCallUpdateFields {
            title: Some(tool_title(&req.tool_name, Some(&req.input))),
            kind: Some(tool_kind(&req.tool_name)),
            locations: tool_locations(&req.tool_name, Some(&req.input)),
            raw_input: Some(req.input.clone()),
            ..Default::default()
        },
    };
    let mut meta_map = Map::new();
    if let Some(r) = reason {
        meta_map.insert(
            "reason".to_string(),
            serde_json::to_value(r).unwrap_or_default(),
        );
    }
    if let Some(d) = delegation {
        meta_map.insert(
            "delegation".to_string(),
            serde_json::to_value(d).unwrap_or_default(),
        );
    }
    let meta = if meta_map.is_empty() {
        None
    } else {
        Some(meta_map)
    };
    RequestPermissionRequest {
        session_id,
        tool_call,
        options: options.to_vec(),
        meta,
    }
}

/// Map the client's permission response to the choice the consent policy
/// settles. None when the client cancelled: the turn ends without a verdict.
/// A selected id approves only when it names an option the plan offered:
/// allow_once approves for this one call, allow_always approves at the plan's
/// scope (only when the plan offered it), and any other id — including a
/// remember id the plan never offered — denies, so a client cannot widen a
/// verdict the card never stated.
pub(crate) fn acp_permission_response_to_decision(
    resp: RequestPermissionResponse,
    planned: &PlannedAsk,
) -> Option<RawConsentChoice> {
    let RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) =
        resp.outcome
    else {
        return None;
    };
    let offered_always = planned
        .options
        .iter()
        .any(|o| o.option_id == "allow_always");
    let (approved, scope) = if option_id == "allow_once" {
        (true, ApprovalScope::Once)
    } else if option_id == "allow_always" && offered_always {
        (true, planned.grant.scope().unwrap_or(ApprovalScope::Once))
    } else {
        (false, ApprovalScope::Once)
    };
    Some(RawConsentChoice {
        approved,
        scope,
        updated_input: None,
    })
}
