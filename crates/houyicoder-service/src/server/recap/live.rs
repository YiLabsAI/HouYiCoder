//! Re-resolving the recap settings while the session runs.
//!
//! The settings file and the session's active model both decide what the
//! recap may do. A change to either reaches the live state through this one
//! path, so the Config tab, the status snapshot, and the trigger never
//! disagree about what the session is running.

use houyicoder_config::ConfigWarning;
use houyicoder_config::load_model_section_from;
use houyicoder_config::recap::load_recap_from;
use tokio::task::spawn_blocking;
use tokio::time::Instant;

use crate::composition::recap_settings;
use crate::server::Server;

impl Server {
    /// Re-resolve the recap settings from the settings file and the session's
    /// current model, then apply them to the live state. Called after the
    /// switch or window is written, and after a model switch, so the runtime
    /// state never lags either source.
    ///
    /// The read runs on the blocking pool: it opens the settings file twice,
    /// and the serve loop is waiting on the reply that depends on it. A panic
    /// in the task leaves the live state as it was, which is the same state
    /// the file had before the change that asked for this refresh.
    pub(crate) async fn refresh_recap(&self) -> Vec<ConfigWarning> {
        let Some(recap) = self.recap.clone() else {
            return Vec::new();
        };
        let settings_path = self.settings_path.clone();
        let session_model = self.runner.active_model();
        let resolved = spawn_blocking(move || {
            let (config, mut warnings) = load_recap_from(&settings_path);
            let (section, section_warnings) = load_model_section_from(&settings_path);
            warnings.extend(section_warnings);
            let (settings, model_warnings) = recap_settings(config, &section, &session_model);
            warnings.extend(model_warnings);
            (settings, warnings)
        })
        .await;
        match resolved {
            Ok((settings, warnings)) => {
                recap.reconfigure(settings, Instant::now());
                warnings
            }
            Err(e) => {
                tracing::warn!("recap settings refresh failed: {e}");
                Vec::new()
            }
        }
    }
}
