//! Workspace trust persistence: a path-keyed map of trusted project
//! directories in user-level settings. The host asks once at startup whether
//! to trust a project folder; the answer persists here so the prompt does
//! not repeat. Ancestor trust covers descendants (trusting a parent trusts
//! its children), mirroring the config.projects[path] walk-up. Lives in
//! user-level settings, never project-local, so a malicious repository
//! cannot self-author trust by shipping a trusted flag in the repo.

use serde_json::Map;
use serde_json::Value;
#[cfg(test)]
use std::env::temp_dir;
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::process::id;

/// True when the project path or any ancestor is recorded as trusted in
/// user-level settings. Walks up from the canonical project path so trusting
/// a parent directory trusts its children without a second ask. False when
/// the path is not trusted or no settings file exists.
pub fn is_path_trusted(settings_path: &Path, project_path: &Path) -> bool {
    let map = read_trusted_map(settings_path);
    let Some(canonical) = canonicalize(project_path) else {
        return false;
    };
    let mut current = canonical.as_path();
    loop {
        if map.contains_key(current.to_string_lossy().as_ref()) {
            return true;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => return false,
        }
    }
}

/// Record a project path as trusted in user-level settings, so future
/// sessions skip the prompt for this path and its descendants. Uses the
/// merge-preserving CAS write so other settings keys round-trip unchanged.
pub fn persist_project_trust(
    settings_path: &Path,
    project_path: &Path,
) -> Result<(), crate::SettingsWriteError> {
    let key = canonicalize(project_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| project_path.to_string_lossy().into_owned());
    crate::update_settings(
        settings_path,
        move |v| {
            let Some(obj) = v.as_object_mut() else {
                return;
            };
            let trusted = obj
                .entry("trusted_projects")
                .or_insert(Value::Object(Map::new()));
            if let Some(trusted_map) = trusted.as_object_mut() {
                trusted_map.insert(key.clone(), Value::Bool(true));
            }
        },
        8,
    )
}

/// Read the trusted_projects map (path -> trusted) from user-level settings.
/// A missing file or missing key yields an empty map (no path trusted).
fn read_trusted_map(settings_path: &Path) -> Map<String, Value> {
    let value = crate::read_settings_value(settings_path);
    value
        .get("trusted_projects")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// Canonicalize a path for use as a trusted_projects key, so the same
/// directory is keyed consistently regardless of how the user typed it.
fn canonicalize(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

#[cfg(test)]
mod tests;
