//! The one place a model pick is applied. Both the between-runs dispatch and
//! the mid-run request handler route through here, so a switch submitted
//! while a turn is in flight takes the same path as one submitted at rest:
//! resolve the Default sentinel, swap model/effort/speed together, persist,
//! and report what the session will actually send.

use houyicoder_config::ModelSection;
use houyicoder_core::agent::{InferenceConfig, Runner};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::model::{
    AppliedModel, EffectiveFrom, ModelApplyResult, ModelCatalog, ModelCatalogEntry, ModelChoice,
    PersistenceOutcome, ResolvedModel, SpeedMode,
};
use houyicoder_protocol::llm::EffortLevel;

use super::Server;
use crate::composition::persist_model_pick;

/// A model pick the host is about to apply, in the terms the wire carries it.
pub(super) struct ModelSelection {
    /// None = the Default sentinel, resolved by the host.
    pub model: Option<String>,
    /// None = the user left effort on auto (follow the resolution chain).
    pub effort: Option<EffortLevel>,
    /// Whether the user touched effort in the picker this time, which the
    /// persistence rule needs (a level equal to the model default is not
    /// written).
    pub effort_toggled: bool,
    /// None = the pick did not express a speed preference, so the session's
    /// current tier stands unless the target model cannot serve it.
    pub speed: Option<SpeedMode>,
    /// Whether a run is in flight when this pick lands.
    pub effective_from: EffectiveFrom,
}

impl ModelSelection {
    /// The same pick carrying the effective level the model will run: what
    /// is persisted must be what runs, never the pre-clamp request.
    fn for_effort(&self, effort: Option<EffortLevel>) -> ModelSelection {
        ModelSelection {
            model: self.model.clone(),
            effort,
            effort_toggled: self.effort_toggled,
            speed: self.speed,
            effective_from: self.effective_from,
        }
    }
}

impl Server {
    /// Apply a model pick: model, effort and speed move together, and the
    /// reply describes what the next request will carry.
    ///
    /// The Default sentinel resolves to the built-in constant rather than
    /// the settings id, because that id is the selection this pick replaces.
    pub(super) async fn apply_model_selection(
        &self,
        selection: ModelSelection,
    ) -> ModelApplyResult {
        let selected = match selection.model.as_deref() {
            Some(id) => ModelChoice::Explicit { id: id.to_string() },
            None => ModelChoice::Default,
        };
        let resolved = match &selected {
            ModelChoice::Default => houyicoder_config::resolve_default_model(),
            ModelChoice::Explicit { id } => id.clone(),
        };
        let capabilities = self.runner.display_capabilities(&resolved);
        let requested = selection
            .speed
            .unwrap_or_else(|| self.runner.active_speed());
        // A session in Fast mode that moves to a model with no fast tier drops
        // to Standard. The pick applies all three dimensions at once, and a
        // tier the model cannot serve is not one of them.
        let speed = if requested == SpeedMode::Fast && !capabilities.fast.is_available() {
            SpeedMode::Standard
        } else {
            requested
        };
        // The level the model will actually run: a pick above the levels the
        // model accepts clamps to the set's top, so the session state, the
        // reply, and the persisted value all carry the same effective level —
        // what is saved is what runs, never a level the model would clamp.
        let effective_effort = houyicoder_core::agent::resolve_applied_effort(
            &resolved,
            selection.effort,
            self.runner.catalog_resolver(),
        );
        // Apply choice, model, effort and speed together under one write. The
        // selection intent is stored, not derived from id equality later, so
        // an explicit pick that resolves to the default id stays Explicit.
        // An auto pick (effort None) stays None in storage — the pane keeps
        // showing "auto", and each request resolves the default at call time
        // so a later default change still leads the session. A concrete pick
        // stores the resolved (ladder-narrowed) level so saved == what runs.
        self.runner.apply_inference(InferenceConfig {
            choice: selected.clone(),
            model: resolved.clone(),
            effort: selection.effort.and(effective_effort),
            speed,
        });

        let persistence = self
            .persist_pick(&selection.for_effort(effective_effort), &resolved, speed)
            .await;
        ModelApplyResult {
            selected,
            applied: AppliedModel {
                id: self.runner.active_model(),
                effort: self.runner.resolve_applied_effort(),
                speed: self.runner.active_speed(),
            },
            effective_from: selection.effective_from,
            persistence,
        }
    }

    /// Persist the pick to settings.json and the session sidecar, reporting
    /// the outcome per destination. The session has already switched when
    /// this runs, so a write failure is a partial result, not a failed pick.
    /// Both writes run on the blocking pool so they do not block a Tokio
    /// worker; the serve loop awaits them, pausing this connection's other
    /// polls for the duration. The pause is bounded by the settings.json
    /// fsync (milliseconds on local disk); the run future is independent so
    /// no deadlock, and under I/O contention the pause grows but the run
    /// resumes once the write resolves.
    async fn persist_pick(
        &self,
        selection: &ModelSelection,
        resolved: &str,
        speed: SpeedMode,
    ) -> PersistenceOutcome {
        let settings_path = self.settings_path.clone();
        let model_input = selection.model.clone();
        let effort = selection.effort;
        let toggled = selection.effort_toggled;
        let store = self.descriptor_store.clone();
        let session = self.session;
        let resolved = resolved.to_string();
        let writes = tokio::task::spawn_blocking(move || {
            let settings = persist_model_pick(
                &settings_path,
                model_input.as_deref(),
                effort,
                toggled,
                speed,
            )
            .map_err(|e| e.to_string());
            let sidecar = Server::write_sidecar_model(store, session, &resolved);
            (settings, sidecar)
        })
        .await;
        // A panic in the write task leaves the outcomes unknown: report the
        // loss rather than guessing either way.
        let (settings, sidecar) = match writes {
            Ok(pair) => pair,
            Err(e) => {
                return PersistenceOutcome::Partial {
                    settings: Some(format!("persistence task failed: {e}")),
                    session_record: Some(format!("persistence task failed: {e}")),
                };
            }
        };
        if settings.is_ok() && sidecar.is_ok() {
            return PersistenceOutcome::Saved;
        }
        // Each destination names its own loss: a settings failure costs new
        // sessions the default, a sidecar failure costs this session its
        // resume.
        PersistenceOutcome::Partial {
            settings: settings.err(),
            session_record: sidecar.err(),
        }
    }

    /// Project the model section plus the live session into the /model pane
    /// snapshot. The settings read is file I/O, so it runs on the blocking
    /// pool so it does not block a Tokio worker. A panic in the projection
    /// fails the reply as an internal error rather than crashing the serve
    /// loop or rendering an empty pane that would read as no models.
    pub(super) async fn model_catalog_snapshot(&self) -> Result<ModelCatalog, ProtocolError> {
        let runner = self.runner.clone();
        let settings_path = self.settings_path.clone();
        let catalog = tokio::task::spawn_blocking(move || {
            let (section, _warnings) = houyicoder_config::load_model_section_from(&settings_path);
            Self::catalog_from_section(&runner, section)
        })
        .await;
        catalog.map_err(|e| {
            ProtocolError::new(
                ErrorCategory::Internal,
                format!("model catalog projection failed: {e}"),
                false,
            )
        })
    }

    /// The catalog rows and the Default resolution come from settings; the
    /// selection, the applied model and the Fast tier come from the runner.
    /// Settings never supply the selection: expressing what the session runs
    /// against the Default sentinel keeps the pane honest after a resume,
    /// when the sidecar model and the settings id can differ.
    fn catalog_from_section(runner: &Runner, section: ModelSection) -> ModelCatalog {
        let applied_id = runner.active_model();
        let default_id = houyicoder_config::resolve_default_model();
        // The selection intent is stored state, not derived from id equality:
        // an explicit pick that resolved to the default id stays Explicit.
        let selected = runner.active_choice();
        let mut entries: Vec<ModelCatalogEntry> = section
            .catalog
            .into_iter()
            .map(|entry| ModelCatalogEntry {
                capabilities: runner.display_capabilities(&entry.id),
                id: entry.id,
                display_name: entry.display_name,
                description: entry.description,
                effort: entry.effort,
            })
            .collect();
        // A session model that is not a catalog row - a --model flag, or a
        // resumed sidecar - still needs a row to sit on, or the pane has no
        // place to show the check and the cursor. Appending keeps the written
        // order untouched. The resolved default already occupies the Default
        // sentinel row, so an applied model equal to it needs no append: the
        // sentinel covers it, and a second bare row would duplicate it.
        if applied_id != default_id && !entries.iter().any(|entry| entry.id == applied_id) {
            entries.push(ModelCatalogEntry {
                capabilities: runner.display_capabilities(&applied_id),
                id: applied_id.clone(),
                display_name: None,
                description: None,
                effort: None,
            });
        }
        ModelCatalog {
            selected,
            applied: AppliedModel {
                id: applied_id,
                effort: runner.resolve_applied_effort(),
                speed: runner.active_speed(),
            },
            resolved_default: ResolvedModel {
                capabilities: runner.display_capabilities(&default_id),
                id: default_id,
            },
            effort_level: section.effort_level,
            entries,
        }
    }
}
