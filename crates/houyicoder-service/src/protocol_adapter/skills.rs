//! Skill entries for the /skills pane.

use houyicoder_api::skill::SkillSnapshot;
use houyicoder_protocol::frontend::skills::{SkillEntry, SkillUsage};

/// Convert discovered skills into the entries the /skills pane renders. Origin
/// carries the discovery source for grouping; invocable carries the
/// model-invocation gate.
pub(crate) fn skill_entries(entries: Vec<SkillSnapshot>) -> Vec<SkillEntry> {
    entries
        .into_iter()
        .map(|snap| SkillEntry {
            name: snap.descriptor.name,
            description: snap.descriptor.description,
            origin: snap.origin,
            invocable: !snap.descriptor.disable_model_invocation,
            user_invocable: snap.descriptor.user_invocable,
            body_token_estimate: snap.descriptor.body_token_estimate,
            usage: Some(SkillUsage {
                invocations: snap.usage.invocations,
                refusals: snap.usage.refusals,
                last_used_secs: snap.usage.last_used_secs,
            }),
        })
        .collect()
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
