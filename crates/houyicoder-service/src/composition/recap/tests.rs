//! Tests for this module.

use super::*;

/// The recap resolves a model without the user naming one. An explicit pick
/// must be a catalog entry that can skip thinking; without one, the session's
/// own model stands in when it can, otherwise the catalog offers the first
/// entry that can, preferring a Fast-tier model. Only a catalog with no such
/// entry leaves the recap off, and that refusal is stated rather than silent.
#[test]
fn test_recap_needs_usable_model() {
    let section = ModelSection {
        catalog: vec![
            ModelEntry {
                id: "small-1".into(),
                ..Default::default()
            },
            ModelEntry {
                id: "gpt-5".into(),
                ..Default::default()
            },
            ModelEntry {
                id: "fast-1".into(),
                fast: Some(true),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let enabled = |model: Option<&str>| RecapConfig {
        enabled: true,
        model: model.map(str::to_string),
        idle_trigger_secs: houyicoder_config::recap::DEFAULT_RECAP_IDLE_SECS,
    };

    let (settings, warnings) = recap_settings(enabled(Some("small-1")), &section, "gpt-5");
    assert_eq!(settings.model.as_deref(), Some("small-1"));
    assert!(
        settings.usable(),
        "a declared model that can skip thinking runs"
    );
    assert!(warnings.is_empty());

    let (settings, warnings) = recap_settings(enabled(Some("gpt-5")), &section, "small-1");
    assert!(
        !settings.usable(),
        "a reasoning-level model keeps the recap off"
    );
    assert_eq!(warnings.len(), 1, "the refusal is stated, not silent");
    assert_eq!(warnings[0].field, "recap.model");

    let (settings, warnings) = recap_settings(enabled(Some("small-9")), &section, "small-1");
    assert!(
        !settings.usable(),
        "an undeclared model keeps the recap off"
    );
    assert_eq!(warnings.len(), 1, "the refusal is stated, not silent");

    // No model named: the session's own model stands in when it can skip
    // thinking, so a fresh install with the switch on runs without extra
    // configuration.
    let (settings, warnings) = recap_settings(enabled(None), &section, "small-1");
    assert_eq!(
        settings.model.as_deref(),
        Some("small-1"),
        "the session model is the first fallback"
    );
    assert!(settings.usable());
    assert!(warnings.is_empty());

    // A session model that cannot skip thinking falls to the catalog, and the
    // Fast-tier entry wins over the plain one.
    let (settings, warnings) = recap_settings(enabled(None), &section, "gpt-5");
    assert_eq!(
        settings.model.as_deref(),
        Some("fast-1"),
        "the catalog's Fast-tier model is preferred"
    );
    assert!(settings.usable());
    assert!(warnings.is_empty());

    let off = RecapConfig {
        enabled: false,
        model: Some("gpt-5".into()),
        idle_trigger_secs: houyicoder_config::recap::DEFAULT_RECAP_IDLE_SECS,
    };
    let (settings, warnings) = recap_settings(off, &section, "small-1");
    assert!(!settings.usable());
    assert!(
        warnings.is_empty(),
        "a disabled section has nothing to refuse"
    );
}

/// A catalog with no entry that can skip thinking leaves the recap off and
/// says so. Silence here would leave the user waiting for a summary that
/// never comes.
#[test]
fn test_recap_without_model_warns() {
    let section = ModelSection {
        catalog: vec![ModelEntry {
            id: "gpt-5".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let config = RecapConfig {
        enabled: true,
        model: None,
        idle_trigger_secs: houyicoder_config::recap::DEFAULT_RECAP_IDLE_SECS,
    };
    let (settings, warnings) = recap_settings(config, &section, "gpt-5");
    assert!(!settings.usable());
    assert_eq!(warnings.len(), 1, "the refusal is stated");
    assert_eq!(warnings[0].field, "recap.model");
}

/// The configured idle window travels into the live settings, so the trigger
/// reads the number the Config tab shows rather than a fixed default.
#[test]
fn test_recap_window_travels() {
    let section = ModelSection {
        catalog: vec![ModelEntry {
            id: "small-1".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let config = RecapConfig {
        enabled: true,
        model: Some("small-1".into()),
        idle_trigger_secs: 180,
    };
    let (settings, _warnings) = recap_settings(config, &section, "small-1");
    assert_eq!(settings.idle_trigger_secs, 180);
    assert_eq!(settings.away_after(), std::time::Duration::from_secs(180));
}
