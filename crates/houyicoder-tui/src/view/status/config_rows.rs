//! The Config tab's rows as a typed list.
//!
//! The tab is a list the user moves through and acts on, so the row order,
//! the label, and the effect of Enter live here rather than in the renderer.
//! Rendering and key handling read the same rows, which is what keeps the
//! highlighted row and the row Enter changes from drifting apart.

use crate::command::render::permission_mode_label;
use crate::state::App;

/// One row of the Config tab, and what Enter does to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ConfigRow {
    /// Which model serves the session. Shown, not edited: the model picker is
    /// its own surface, and a list whose rows changed the page would be the
    /// only such row here.
    Model,
    /// The reasoning effort actually sent with requests.
    ReasoningEffort,
    /// The permission mode the gate applies.
    PermissionMode,
    /// How long the approval card counts down before approving once.
    AutoApproveTimer,
    /// The sandbox the session runs under.
    Sandbox,
    /// Whether the session records memories on its own.
    AutoMemory,
    /// Whether the session writes a recap while the user is away.
    AutoDream,
    /// Whether the host may write a recap at all.
    Recap,
    /// How long the session must sit idle before that recap is written.
    RecapIdle,
}

/// The rows in display order. This is the order the reader sees and the order
/// the cursor moves through.
pub(crate) const CONFIG_ROWS: [ConfigRow; 9] = [
    ConfigRow::Model,
    ConfigRow::ReasoningEffort,
    ConfigRow::PermissionMode,
    ConfigRow::AutoApproveTimer,
    ConfigRow::Sandbox,
    ConfigRow::AutoMemory,
    ConfigRow::AutoDream,
    ConfigRow::Recap,
    ConfigRow::RecapIdle,
];

/// The delay values the auto-approve timer cycles through, in seconds. Zero
/// means off. The order runs from the working default outward, so the first
/// press on a fresh session lands on the next plausible setting rather than
/// jumping to an extreme.
pub(crate) const TIMER_STEPS: [u64; 6] = [7, 15, 30, 0, 3, 5];

/// The idle windows the recap row cycles through, in seconds. The first press
/// moves to the next plausible window rather than jumping to an extreme, the
/// same ordering the timer row uses.
pub(crate) const RECAP_IDLE_STEPS: [u64; 4] = [180, 300, 600, 900];

/// Whether a row acts on Enter. A row that shows state the session does not
/// own (the model, the sandbox) takes no press, so it prints no hint.
pub(crate) fn row_changes(row: ConfigRow) -> bool {
    !matches!(row, ConfigRow::Model | ConfigRow::Sandbox)
}

/// The label a row prints, capitalized at the first word so the column reads
/// as one kind of thing.
pub(crate) fn row_label(row: ConfigRow) -> &'static str {
    match row {
        ConfigRow::Model => "Model",
        ConfigRow::ReasoningEffort => "Reasoning effort",
        ConfigRow::PermissionMode => "Permission mode",
        ConfigRow::AutoApproveTimer => "Auto-approve timer",
        ConfigRow::Sandbox => "Sandbox",
        ConfigRow::AutoMemory => "Auto-memory",
        ConfigRow::AutoDream => "Auto-dream",
        ConfigRow::Recap => "Recap",
        ConfigRow::RecapIdle => "Recap idle trigger",
    }
}

/// The value a row prints, as the row currently reads.
pub(crate) fn row_value(app: &App, row: ConfigRow) -> String {
    let on_off = |b: bool| {
        if b {
            "on".to_string()
        } else {
            "off".to_string()
        }
    };
    match row {
        ConfigRow::Model => app
            .sessions
            .current
            .capabilities
            .model
            .catalog
            .applied
            .id
            .clone(),
        ConfigRow::ReasoningEffort => app
            .sessions
            .current
            .capabilities
            .model
            .catalog
            .applied
            .effort
            .map(|effort| effort.label().to_string())
            .unwrap_or_else(|| "(default)".to_string()),
        ConfigRow::PermissionMode => permission_mode_label(app.current_mode()).to_string(),
        ConfigRow::AutoApproveTimer => {
            let delay = app
                .sessions
                .current
                .capabilities
                .permission
                .auto_approve_delay_secs;
            if delay == 0 {
                "off".to_string()
            } else {
                format!("{delay}s")
            }
        }
        ConfigRow::Sandbox => app.sessions.current.sandbox.clone(),
        ConfigRow::AutoMemory => on_off(app.snapshot_or_stub().auto_memory),
        ConfigRow::AutoDream => on_off(app.snapshot_or_stub().auto_dream),
        ConfigRow::Recap => on_off(app.snapshot_or_stub().recap_enabled),
        ConfigRow::RecapIdle => {
            let secs = app.snapshot_or_stub().recap_idle_secs;
            if secs == 0 {
                "off".to_string()
            } else {
                format!("{}m", secs / 60)
            }
        }
    }
}

#[cfg(test)]
mod tests;
