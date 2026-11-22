//! Protocol-agnostic consent kernel: the durable grant an approved answer
//! authorizes, the options the approval card offers for it, and the install
//! that widens the fence and persists the rule the gate keeps. Both the
//! frontend server and the ACP server install the same grant for the same
//! answer by calling these functions.

mod grant_install;
mod policy;

pub(crate) use grant_install::{
    ConsentInstall, apply_consent_directory, apply_consent_rule, approval_directory_grants,
    card_fence, consent_plan, install_consent, install_directory_grants, route_consent,
};
pub(crate) use policy::{CardFence, DurableGrant, approval_options, installed_scope};
#[cfg(test)]
pub(crate) use policy::{FENCED, UNFENCED, consent_rule_for, durable_grant};
