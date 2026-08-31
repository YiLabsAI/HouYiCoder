//! Workspace trust persistence: a path-keyed map of trusted project
//! directories in user-level settings. The host asks once at startup whether
//! to trust a project folder; the answer persists here so the prompt does
//! not repeat. Ancestor trust covers descendants (trusting a parent trusts
//! its children), mirroring the config.projects[path] walk-up. Lives in
//! user-level settings, never project-local, so a malicious repository
//! cannot self-author trust by shipping a trusted flag in the repo.

use std::path::Path;

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
                .or_insert(serde_json::Value::Object(serde_json::Map::new()));
            if let Some(trusted_map) = trusted.as_object_mut() {
                trusted_map.insert(key.clone(), serde_json::Value::Bool(true));
            }
        },
        8,
    )
}

/// Read the trusted_projects map (path -> trusted) from user-level settings.
/// A missing file or missing key yields an empty map (no path trusted).
fn read_trusted_map(settings_path: &Path) -> serde_json::Map<String, serde_json::Value> {
    let value = crate::read_settings_value(settings_path);
    value
        .get("trusted_projects")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// Canonicalize a path for use as a trusted_projects key, so the same
/// directory is keyed consistently regardless of how the user typed it.
fn canonicalize(path: &Path) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn settings_file(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("trust-cfg-{tag}-{}", std::process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    /// Persisting a path then checking it returns true, and the write
    /// preserves other keys in the settings file.
    #[test]
    fn test_persist_then_trusted() {
        let path = settings_file("roundtrip");
        // Seed an unrelated key so the merge-preserving write must keep it.
        fs::write(&path, r#"{"model":{"id":"opencoder"}}"#).unwrap();
        let proj = std::env::temp_dir().join("trust-proj-roundtrip");
        drop(fs::remove_dir_all(&proj));
        fs::create_dir_all(&proj).unwrap();

        assert!(!is_path_trusted(&path, &proj), "not trusted before persist");
        persist_project_trust(&path, &proj).unwrap();
        assert!(is_path_trusted(&path, &proj), "trusted after persist");

        // The unrelated key survives the merge-preserving write.
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["model"]["id"], "opencoder", "other keys preserved");

        drop(fs::remove_dir_all(path.parent().expect("parent exists")));
        drop(fs::remove_dir_all(&proj));
    }

    /// Trusting a parent directory trusts its children without a second ask
    /// (ancestor walk-up): persist the parent, ask about a child, get true.
    #[test]
    fn test_ancestor_trust_covers_child() {
        let path = settings_file("ancestor");
        let root = std::env::temp_dir().join("trust-ancestor-root");
        drop(fs::remove_dir_all(&root));
        let parent = root.join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();

        persist_project_trust(&path, &parent).unwrap();
        assert!(
            is_path_trusted(&path, &child),
            "trusting the parent must cover the child"
        );
        assert!(is_path_trusted(&path, &parent), "parent itself trusted");

        drop(fs::remove_dir_all(&root));
        drop(fs::remove_dir_all(path.parent().unwrap()));
    }

    /// A path with no recorded trust returns false, and a missing settings
    /// file also returns false (no crash, no trust).
    #[test]
    fn test_untrusted_returns_false() {
        let path = settings_file("none");
        let proj = std::env::temp_dir().join("trust-proj-none");
        drop(fs::remove_dir_all(&proj));
        fs::create_dir_all(&proj).unwrap();

        assert!(!is_path_trusted(&path, &proj), "no trust recorded");

        // A settings file that does not exist at all: still no crash, false.
        let missing = path.parent().unwrap().join("absent.json");
        assert!(
            !is_path_trusted(&missing, &proj),
            "missing settings file yields false, not a crash"
        );

        drop(fs::remove_dir_all(proj));
        drop(fs::remove_dir_all(path.parent().unwrap()));
    }
}
