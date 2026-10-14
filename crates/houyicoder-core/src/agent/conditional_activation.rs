//! Session-scoped state + matcher for paths-gated skills.
//!
//! A skill with a non-empty paths list is conditional: hidden from the
//! listing until a file-touch tool touches a matching path, then active
//! for the rest of the session. State lives on the Runner (one per
//! session), not a global, so activations do not leak across sessions.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use houyicoder_api::skill::SkillRegistry;
use ignore::gitignore::GitignoreBuilder;
#[cfg(test)]
use std::env::temp_dir;
#[cfg(test)]
use std::fs::create_dir_all;
#[cfg(test)]
use std::process::id;

/// Activation seam the file-touch tools call and the listing step reads.
/// Engine-local: every consumer lives in the engine, so lifting it to the
/// port widens the surface for no inversion gain.
pub trait ConditionalSkillActivator: Send + Sync {
    /// Activate skills whose globs match; return the newly activated.
    fn activate_for_paths(&self, file_paths: &[String]) -> Vec<String>;

    /// Whether a conditional skill is active this session.
    fn is_active(&self, name: &str) -> bool;

    /// Re-derive the conditional set + origins from the registry after a
    /// hot reload, without dropping the active set (a skill already
    /// activated this session stays active). No-op default: a stub or test
    /// activator that does not back its set with a registry keeps the
    /// construction-time view.
    fn refresh(&self) {}
}

/// Session-scoped activation state + matcher. The conditional set and
/// origin map are re-derived from the registry on a hot reload (so a
/// newly-added conditional skill is recognized) while the active set
/// persists across the reload (an already-activated skill stays visible).
pub struct ConditionalActivation {
    active: Arc<Mutex<HashSet<String>>>,
    cwd: PathBuf,
    /// Backing registry, held so refresh can re-derive without the caller
    /// passing it back in.
    registry: Arc<dyn SkillRegistry>,
    /// Skills with non-empty paths, re-derived on refresh.
    conditional: RwLock<Vec<(String, Vec<String>)>>,
    /// name -> origin label, for the activation trace. Re-derived on refresh.
    origins: RwLock<HashMap<String, String>>,
}

impl ConditionalActivation {
    /// Read the conditional set + origin map once at construction.
    pub fn new(registry: Arc<dyn SkillRegistry>, cwd: PathBuf) -> Self {
        let (conditional, origins) = derive_conditional(&registry);
        Self {
            active: Arc::new(Mutex::new(HashSet::new())),
            cwd,
            registry,
            conditional: RwLock::new(conditional),
            origins: RwLock::new(origins),
        }
    }

    /// The matcher base (workspace root).
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Re-derive the conditional set + origins from the registry, leaving
    /// the active set untouched. Called by the hot-reload driver after the
    /// registry's own reload swapped in a fresh skill set.
    pub fn refresh(&self) {
        let (conditional, origins) = derive_conditional(&self.registry);
        let mut c = self.conditional.write().unwrap_or_else(|e| e.into_inner());
        let mut o = self.origins.write().unwrap_or_else(|e| e.into_inner());
        *c = conditional;
        *o = origins;
    }
}

/// A conditional skill's name + its paths globs.
type ConditionalSet = Vec<(String, Vec<String>)>;
/// name -> origin label, for the activation trace.
type OriginMap = HashMap<String, String>;

/// Derive the conditional set (name, paths) and the name-to-origin map
/// from the registry. Pure over the registry's current view, so the same
/// call seeds construction and refreshes after a reload.
fn derive_conditional(registry: &Arc<dyn SkillRegistry>) -> (ConditionalSet, OriginMap) {
    let origins: HashMap<String, String> = registry
        .list_with_origin()
        .into_iter()
        .map(|s| (s.descriptor.name, s.origin))
        .collect();
    let conditional: Vec<(String, Vec<String>)> = registry
        .list_model_invocable()
        .into_iter()
        .filter_map(|d| {
            let paths = registry.paths_for(&d.name);
            if paths.is_empty() {
                None
            } else {
                Some((d.name, paths))
            }
        })
        .collect();
    (conditional, origins)
}

impl ConditionalSkillActivator for ConditionalActivation {
    fn activate_for_paths(&self, file_paths: &[String]) -> Vec<String> {
        let conditional = self.conditional.read().unwrap_or_else(|e| e.into_inner());
        let origins = self.origins.read().unwrap_or_else(|e| e.into_inner());
        if conditional.is_empty() {
            return Vec::new();
        }
        let mut newly_activated = Vec::new();
        for (name, globs) in conditional.iter() {
            if self.is_active(name) {
                continue;
            }
            // One matcher per skill, globs in order so negation holds.
            // add_line takes a glob; ::add opens a file path and fails
            // silently as Option<Error>.
            let mut builder = GitignoreBuilder::new(&self.cwd);
            let mut bad = false;
            for g in globs {
                if let Err(e) = builder.add_line(None, g) {
                    tracing::warn!(skill = %name, glob = %g, error = %e, "invalid paths glob; skill stays conditional");
                    bad = true;
                    break;
                }
            }
            if bad {
                continue;
            }
            let matcher = match builder.build() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(skill = %name, error = %e, "paths matcher build failed; skill stays conditional");
                    continue;
                }
            };
            for fp in file_paths {
                // Panic guard: matched_path_or_any_parents asserts the path
                // is under the matcher root.
                let Some(rel) = relative_under_cwd(&self.cwd, fp) else {
                    continue;
                };
                // Walks parents so a bare dir (src) matches a descendant
                // (src/foo.rs). ::matched skips parents.
                if matcher.matched_path_or_any_parents(&rel, false).is_ignore() {
                    let mut active = self.active.lock().expect("active set not poisoned");
                    if active.insert(name.clone()) {
                        let origin = origins.get(name).map(String::as_str).unwrap_or("unknown");
                        tracing::info!(
                            skill = %name,
                            origin = %origin,
                            file = %rel.display(),
                            "conditional skill activated for the session",
                        );
                        newly_activated.push(name.clone());
                    }
                    break;
                }
            }
        }
        newly_activated
    }

    fn is_active(&self, name: &str) -> bool {
        self.active
            .lock()
            .expect("active set not poisoned")
            .contains(name)
    }

    fn refresh(&self) {
        ConditionalActivation::refresh(self);
    }
}

/// Path relative to cwd, or None when the file is not under cwd. The skip
/// is a panic guard for the matcher and is semantically correct: a file
/// outside cwd cannot match cwd-relative patterns.
fn relative_under_cwd(cwd: &Path, file: &str) -> Option<PathBuf> {
    let p = Path::new(file);
    // Absolute paths (platform-native) must strip under cwd. On Windows,
    // a Unix-style /etc/passwd is NOT absolute (no drive), so it falls to
    // the relative branch — but it still escapes cwd, so the starts_with
    // guard below catches it.
    let rel = if p.is_absolute() {
        if !p.starts_with(cwd) {
            return None;
        }
        p.strip_prefix(cwd).ok()?.to_path_buf()
    } else {
        p.to_path_buf()
    };
    let s = rel.to_string_lossy();
    if s.is_empty() || s.starts_with("..") || s.starts_with('/') || rel.is_absolute() {
        return None;
    }
    Some(rel)
}

#[cfg(test)]
mod tests;
