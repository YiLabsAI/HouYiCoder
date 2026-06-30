//! The /model pane's catalog snapshot: the entries the pane lists + the
//! active id + the global effort fallback. Mirrors the config layer's
//! ModelSection shape (id + effort_level + catalog entries) so the host
//! renders without importing the config crate. The pane lists catalog
//! entries in their written order (a Vec, not a map) so the author's
//! intended row order survives the wire.

use crate::llm::EffortLevel;

/// One row the /model pane lists. The id is what the provider sees; the
/// display_name + description are pane-only copy (fall back to the id when
/// unset). effort is the persisted per-model pick (None = follow the
/// resolution chain).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub effort: Option<EffortLevel>,
}

/// The /model pane snapshot: the active id (None = the Default sentinel,
/// resolved by the host through the settings→DEFAULT chain), a global effort
/// fallback for entries without one, and the catalog rows in written order.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelCatalog {
    #[serde(default)]
    pub active_id: Option<String>,
    #[serde(default)]
    pub effort_level: Option<EffortLevel>,
    #[serde(default)]
    pub catalog: Vec<ModelCatalogEntry>,
}
