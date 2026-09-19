//! The /model pane snapshot: selection intent, the model the session will
//! actually send, the model the Default row resolves to, and the catalog
//! rows in written order. Mirrors the config layer's model section so the
//! host renders without importing the config crate.
//!
//! The three concepts stay separate because collapsing them into one id
//! lets the pane show a settings value while the runner sends another.

use crate::llm::EffortLevel;

/// The user's selection intent. Default means "follow the resolver", not
/// "whatever the settings id currently holds" - the two differ the moment
/// the user picks Default while settings still names a concrete id.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum ModelChoice {
    #[default]
    Default,
    Explicit {
        id: String,
    },
}

impl ModelChoice {
    /// The concrete id this choice names, or None for the Default sentinel
    /// (the host resolves it).
    pub fn explicit_id(&self) -> Option<&str> {
        match self {
            ModelChoice::Default => None,
            ModelChoice::Explicit { id } => Some(id.as_str()),
        }
    }
}

/// The speed tier a request runs at. A two-state enum rather than a bool so
/// a later tier lands without a wire reshape.
pub use crate::llm::SpeedMode;

/// What the next model request will carry: the resolved id, the effort the
/// host will emit (None = no effort parameter), and the speed tier. Read
/// from the live session so the pane and status bar report the applied state
/// rather than a guess at it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppliedModel {
    pub id: String,
    #[serde(default)]
    pub effort: Option<EffortLevel>,
    #[serde(default)]
    pub speed: SpeedMode,
}

/// Where a resolved context window came from. Carried for status
/// diagnostics; the pane renders only the token count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextWindowSource {
    /// The provider negotiated the window.
    Provider,
    /// An enforced limit learned from a provider overflow rejection.
    Learned,
    /// The user set the per-model context window in the catalog.
    ExplicitConfig,
    /// A model-id suffix opted into a wider window.
    ModelSuffix,
    /// The shipped per-family table.
    ModelCatalog,
    /// The built-in conservative default.
    Fallback,
}

/// A resolved context window plus its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextWindow {
    pub tokens: u32,
    pub source: ContextWindowSource,
}

/// Whether the focused model accepts an effort parameter, and the levels it
/// accepts: the dialect's verified set, minus any the catalog's per-model
/// list excludes. Unsupported says only that the model speaks no effort
/// dialect - never that the model is invalid or unusable.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum EffortCapability {
    Supported { levels: Vec<EffortLevel> },
    Unsupported,
}

/// Whether the focused model can run in Fast mode, and why not when it
/// cannot. The host decides; the pane never guesses from the model id.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum FastModeAvailability {
    Available,
    /// The catalog declares no fast tier for this model at all (the common
    /// default). The pane hides the setting rather than printing noise.
    NotConfigured,
    Unavailable {
        reason: String,
    },
    Cooldown {
        reason: String,
        reset_at_ms: u64,
    },
}

impl FastModeAvailability {
    /// Whether the setting can take focus at all.
    pub fn is_available(&self) -> bool {
        matches!(self, FastModeAvailability::Available)
    }

    /// The reason to show when the setting cannot take focus.
    pub fn reason(&self) -> Option<&str> {
        match self {
            FastModeAvailability::Available | FastModeAvailability::NotConfigured => None,
            FastModeAvailability::Unavailable { reason }
            | FastModeAvailability::Cooldown { reason, .. } => Some(reason.as_str()),
        }
    }
}

/// The host-resolved display capabilities for one model. The pane renders
/// these instead of probing the model id for substrings: what a model
/// supports is the host's answer, not a guest-side guess.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelDisplayCapabilities {
    #[serde(default)]
    pub context_window: Option<ContextWindow>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    pub effort: EffortCapability,
    pub fast: FastModeAvailability,
}

impl Default for ModelDisplayCapabilities {
    fn default() -> Self {
        Self {
            context_window: None,
            max_output_tokens: None,
            effort: EffortCapability::Unsupported,
            fast: FastModeAvailability::NotConfigured,
        }
    }
}

/// One row the /model pane lists: identity and presentation, plus the
/// host-resolved capabilities. The id is what the provider sees;
/// display_name and description are pane-only copy (falling back to the id
/// when unset). effort is the persisted per-model pick (None = follow the
/// resolution chain).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub effort: Option<EffortLevel>,
    #[serde(default)]
    pub capabilities: ModelDisplayCapabilities,
}

impl ModelCatalogEntry {
    /// The label the pane prints for this row.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(self.id.as_str())
    }
}

/// The model the Default row resolves to right now, with its capabilities.
/// Exists to explain the Default row; selecting Default applies this choice
/// rather than re-reading the settings id it is about to delete.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedModel {
    pub id: String,
    #[serde(default)]
    pub capabilities: ModelDisplayCapabilities,
}

/// The /model pane snapshot. selected is the user's intent, applied is what
/// the live session will send, resolved_default explains the Default row,
/// effort_level is the global fallback for entries without a per-model
/// effort, and entries are the catalog rows in written order.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelCatalog {
    #[serde(default)]
    pub selected: ModelChoice,
    pub applied: AppliedModel,
    #[serde(default)]
    pub resolved_default: ResolvedModel,
    #[serde(default)]
    pub effort_level: Option<EffortLevel>,
    #[serde(default)]
    pub entries: Vec<ModelCatalogEntry>,
}

/// When an applied pick takes effect. A switch submitted while a run is in
/// flight cannot change the request already handed to the provider, so the
/// receipt says so instead of implying an immediate swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveFrom {
    /// No run is in flight: the next constructed request carries this pick
    /// with nothing ahead of it.
    Immediate,
    /// A run is in flight: the request already sent keeps its configuration
    /// and the next request not yet constructed carries this pick.
    NextRequest,
}

/// Whether the pick reached disk. A settings or descriptor write failure must
/// not masquerade as a complete success: the session did switch, but a
/// restart would lose it. Each destination reports its own failure, so the
/// guest can name the loss rather than parse a merged string. The field is
/// required: a payload without it fails to decode rather than read as Saved.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum PersistenceOutcome {
    Saved,
    Partial {
        settings: Option<String>,
        session_record: Option<String>,
    },
}

/// The reply to a model set: the selection that was applied, the model the
/// session will now send, when it takes effect, and whether it persisted.
/// The transcript formats its line from this, never from the draft, so an
/// unapplied or unpersisted pick cannot render as a success.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelApplyResult {
    pub selected: ModelChoice,
    pub applied: AppliedModel,
    pub effective_from: EffectiveFrom,
    pub persistence: PersistenceOutcome,
}
