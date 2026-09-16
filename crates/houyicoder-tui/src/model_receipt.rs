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
mod tests {
    use super::*;
    use houyicoder_protocol::frontend::model::{AppliedModel, ModelChoice};
    use houyicoder_protocol::llm::EffortLevel;

    fn result(id: &str, effort: Option<EffortLevel>, speed: SpeedMode) -> ModelApplyResult {
        ModelApplyResult {
            selected: ModelChoice::Explicit { id: id.into() },
            applied: AppliedModel {
                id: id.into(),
                effort,
                speed,
            },
            effective_from: EffectiveFrom::Immediate,
            persistence: PersistenceOutcome::Saved,
        }
    }

    /// The receipt names the id the provider sees, not the label alone: the
    /// display name is how the pick was presented, the id is what runs.
    #[test]
    fn test_model_receipt_names_actual() {
        let line = receipt_line(
            &result("qwen3.8-max", Some(EffortLevel::High), SpeedMode::Standard),
            "Max",
            SpeedMode::Standard,
            false,
        );
        assert_eq!(line, "Model set to Max (qwen3.8-max) · high effort");
    }

    /// The Default sentinel reads as the choice next to the model it resolved
    /// to, so the receipt explains the pick instead of repeating the settings
    /// id it replaced.
    #[test]
    fn test_model_receipt_default_target() {
        let mut applied = result("qwen3.7-max", None, SpeedMode::Standard);
        applied.selected = ModelChoice::Default;
        let line = receipt_line(&applied, "Default", SpeedMode::Standard, false);
        assert_eq!(line, "Model set to Default (qwen3.7-max)");
    }

    /// The Fast segment appears when the tier moved, or when the applied model
    /// serves one at all; a tier that never existed for this model is not
    /// worth a line, one the session keeps at the same value is not news.
    #[test]
    fn test_model_receipt_fast_segment() {
        let moved = receipt_line(
            &result("glm-5.2", None, SpeedMode::Fast),
            "Fable",
            SpeedMode::Standard,
            false,
        );
        assert_eq!(moved, "Model set to Fable (glm-5.2) · Fast mode on");
        let unchanged = receipt_line(
            &result("glm-5.2", None, SpeedMode::Standard),
            "Fable",
            SpeedMode::Standard,
            false,
        );
        assert_eq!(unchanged, "Model set to Fable (glm-5.2)");
        let served = receipt_line(
            &result("glm-5.2", None, SpeedMode::Standard),
            "Fable",
            SpeedMode::Standard,
            true,
        );
        assert_eq!(served, "Model set to Fable (glm-5.2) · Fast mode off");
        // The tier was on and the focus moved to a model that cannot serve it:
        // the receipt writes the change the downgrade made.
        let dropped = receipt_line(
            &result("plain-model", None, SpeedMode::Standard),
            "Plain",
            SpeedMode::Fast,
            false,
        );
        assert_eq!(dropped, "Model set to Plain (plain-model) · Fast mode off");
    }

    /// A pick that reached the session but not disk carries each failed
    /// destination's reason verbatim.
    #[test]
    fn test_model_receipt_not_saved() {
        let mut applied = result("glm-5.2", None, SpeedMode::Standard);
        applied.persistence = PersistenceOutcome::Partial {
            settings: Some("read-only settings".into()),
            session_record: None,
        };
        let line = receipt_line(&applied, "Fable", SpeedMode::Standard, false);
        assert_eq!(
            line,
            "Model set to Fable (glm-5.2) · settings not saved: read-only settings"
        );
    }

    /// When both destinations failed the receipt keeps both parts: one loss
    /// must not mask the other.
    #[test]
    fn test_model_receipt_both_losses() {
        let mut applied = result("glm-5.2", None, SpeedMode::Standard);
        applied.persistence = PersistenceOutcome::Partial {
            settings: Some("e1".into()),
            session_record: Some("e2".into()),
        };
        let line = receipt_line(&applied, "Fable", SpeedMode::Standard, false);
        assert!(
            line.contains("settings not saved: e1")
                && line.contains("session record not saved: e2"),
            "both losses reach the transcript: {line}"
        );
    }

    /// A pick that landed while a request was in flight names the boundary it
    /// takes effect on, so the running request is not implied to have changed.
    #[test]
    fn test_model_receipt_names_boundary() {
        let mut applied = result("glm-5.2", None, SpeedMode::Standard);
        applied.effective_from = EffectiveFrom::NextRequest;
        let line = receipt_line(&applied, "Fable", SpeedMode::Standard, false);
        assert_eq!(
            line,
            "Model set to Fable (glm-5.2) · applies to the next model request"
        );
    }
}
