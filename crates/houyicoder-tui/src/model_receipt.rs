//! The transcript receipt for a model pick, formatted from the host's apply
//! result rather than from the draft. The line names the display label and
//! the id the session will actually send, says where the pick takes effect,
//! and says so when it did not reach disk.

use houyicoder_protocol::frontend::model::{
    EffectiveFrom, ModelApplyResult, PersistenceOutcome, SpeedMode,
};

/// The receipt line for an applied pick. label is the display name the picker
/// showed, prior_speed the tier the session ran at before the pick, and
/// fast_available whether the applied model can serve Fast at all - a tier
/// that never existed for this model is not worth a line, one that moved is.
pub(crate) fn receipt_line(
    result: &ModelApplyResult,
    label: &str,
    prior_speed: SpeedMode,
    fast_available: bool,
) -> String {
    let mut line = format!("Model set to {label} ({})", result.applied.id);
    if let Some(effort) = result.applied.effort {
        line.push_str(&format!(" · {} effort", effort.label()));
    }
    if fast_available || result.applied.speed != prior_speed {
        line.push_str(&format!(" · Fast mode {}", result.applied.speed.label()));
    }
    if result.effective_from == EffectiveFrom::NextRequest {
        line.push_str(" · applies to the next model request");
    }
    // Each destination names its own loss, so the receipt reports both
    // verbatim instead of a merged claim that may not fit.
    if let PersistenceOutcome::Partial {
        settings,
        session_record,
    } = &result.persistence
    {
        let mut parts = Vec::new();
        if let Some(e) = settings {
            parts.push(format!("settings not saved: {e}"));
        }
        if let Some(e) = session_record {
            parts.push(format!("session record not saved: {e}"));
        }
        if !parts.is_empty() {
            line.push_str(&format!(" · {}", parts.join("; ")));
        }
    }
    line
}

#[cfg(test)]
mod tests;
