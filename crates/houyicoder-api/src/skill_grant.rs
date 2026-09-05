//! Per-skill sandbox grant store. Maps skill names to authorized mach
//! services. User-scope only; separate from the rule store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Apple system services never authorizable through any grant path.
pub const DENIED_MACH_SERVICES: &[&str] = &[
    "com.apple.pasteboard",
    "com.apple.cfprefsd",
    "com.apple.coreservices.appleevents",
];

pub struct SkillGrantStore {
    grants: Mutex<HashMap<String, Vec<String>>>,
    path: PathBuf,
}

impl SkillGrantStore {
    pub fn new() -> Self {
        let path = Self::path();
        Self {
            grants: Mutex::new(load_grants(&path)),
            path,
        }
    }

    pub fn path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".houyicoder").join("skill-grants.json")
    }

    pub fn grant_for(&self, skill: &str) -> Vec<String> {
        self.grants
            .lock()
            .expect("grant lock poisoned")
            .get(skill)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !DENIED_MACH_SERVICES.contains(&s.as_str()))
            .collect()
    }

    pub fn set_grant(&self, skill: &str, services: Vec<String>) {
        let filtered: Vec<String> = services
            .into_iter()
            .filter(|s| !DENIED_MACH_SERVICES.contains(&s.as_str()))
            .collect();
        let mut grants = self.grants.lock().expect("grant lock poisoned");
        grants.insert(skill.to_string(), filtered);
        save_grants(&self.path, &grants);
    }

    /// Merge granted services with the skill's frontmatter declaration.
    /// Union with dedup. The Apple deny-list applies to both halves —
    /// frontmatter is a grant path too, so its services are filtered.
    pub fn merged_services(&self, skill: &str, frontmatter: &[String]) -> Vec<String> {
        let grants = self.grant_for(skill);
        let mut mach: Vec<String> = frontmatter
            .iter()
            .filter(|s| !DENIED_MACH_SERVICES.contains(&s.as_str()))
            .cloned()
            .collect();
        for s in grants {
            if !mach.contains(&s) {
                mach.push(s);
            }
        }
        mach
    }
}

impl Default for SkillGrantStore {
    fn default() -> Self {
        Self::new()
    }
}

fn load_grants(path: &Path) -> HashMap<String, Vec<String>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, Vec<String>>>(&text) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("skill-grants parse error: {e}");
            HashMap::new()
        }
    }
}

fn save_grants(path: &Path, grants: &HashMap<String, Vec<String>>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent).is_ok();
    }
    match serde_json::to_string_pretty(grants) {
        Ok(text) => {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, &text).is_ok() {
                if let Err(e) = std::fs::rename(&tmp, path) {
                    tracing::warn!("skill-grants rename failed: {e}");
                }
            } else {
                tracing::warn!("skill-grants tmp write failed");
            }
        }
        Err(e) => tracing::warn!("skill-grants serialize failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grant_unknown_empty() {
        let store = SkillGrantStore {
            grants: Mutex::new(HashMap::new()),
            path: std::env::temp_dir().join("grant-unknown.json"),
        };
        assert!(store.grant_for("nope").is_empty());
    }

    #[test]
    fn test_disk_round_trip() {
        let dir = std::env::temp_dir().join("grant-rt");
        let _ = std::fs::create_dir_all(&dir).is_ok();
        let path = dir.join("skill-grants.json");
        {
            let store = SkillGrantStore {
                grants: Mutex::new(HashMap::new()),
                path: path.clone(),
            };
            store.set_grant("ego-browser", vec!["a.b.c".into()]);
        }
        let reloaded = SkillGrantStore {
            grants: Mutex::new(load_grants(&path)),
            path: path.clone(),
        };
        assert_eq!(reloaded.grant_for("ego-browser"), vec!["a.b.c".to_string()]);
        let _ = std::fs::remove_dir_all(&dir).is_ok();
    }

    #[test]
    fn test_set_and_read() {
        let dir = std::env::temp_dir().join("grant-test");
        let _ = std::fs::create_dir_all(&dir).is_ok();
        let path = dir.join("skill-grants.json");
        let store = SkillGrantStore {
            grants: Mutex::new(load_grants(&path)),
            path,
        };
        store.set_grant("ego-browser", vec!["a.b.c".into()]);
        assert_eq!(store.grant_for("ego-browser"), vec!["a.b.c".to_string()]);
        let _ = std::fs::remove_dir_all(&dir).is_ok();
    }

    #[test]
    fn test_denied_filtered() {
        let store = SkillGrantStore {
            grants: Mutex::new(HashMap::new()),
            path: std::env::temp_dir().join("grant-denied.json"),
        };
        store.set_grant("s1", vec!["com.apple.pasteboard".into(), "a.b.c".into()]);
        assert!(
            !store
                .grant_for("s1")
                .contains(&"com.apple.pasteboard".to_string())
        );
        assert!(store.grant_for("s1").contains(&"a.b.c".to_string()));
    }

    #[test]
    fn test_corrupt_file_returns_empty() {
        let dir = std::env::temp_dir().join("grant-corrupt");
        let _ = std::fs::create_dir_all(&dir).is_ok();
        let path = dir.join("skill-grants.json");
        let _ = std::fs::write(&path, "not valid json {{{").is_ok();
        let store = SkillGrantStore {
            grants: Mutex::new(load_grants(&path)),
            path: path.clone(),
        };
        assert!(store.grant_for("any").is_empty());
        let _ = std::fs::remove_dir_all(&dir).is_ok();
    }

    #[test]
    fn test_merged_deny_both_halves() {
        let dir = std::env::temp_dir().join("grant-merge");
        let _ = std::fs::create_dir_all(&dir).is_ok();
        let path = dir.join("skill-grants.json");
        let store = SkillGrantStore {
            grants: Mutex::new(load_grants(&path)),
            path: path.clone(),
        };
        // Grant store side filtered (pasteboard blocked).
        store.set_grant("s1", vec!["com.apple.pasteboard".into(), "x.y.z".into()]);
        // Frontmatter side with a deny-listed service + an overlap entry.
        let frontmatter = vec!["com.apple.cfprefsd".into(), "x.y.z".into(), "a.b.c".into()];
        let merged = store.merged_services("s1", &frontmatter);
        // Deny-list entries from both halves gone.
        assert!(!merged.contains(&"com.apple.pasteboard".to_string()));
        assert!(!merged.contains(&"com.apple.cfprefsd".to_string()));
        // Union of survivors, no dup from overlap.
        assert_eq!(merged, vec!["x.y.z".to_string(), "a.b.c".to_string()]);
        let _ = std::fs::remove_dir_all(&dir).is_ok();
    }

    #[test]
    fn test_merged_frontmatter_only() {
        let store = SkillGrantStore {
            grants: Mutex::new(HashMap::new()),
            path: std::env::temp_dir().join("grant-noop.json"),
        };
        let frontmatter = vec!["a.b.c".into(), "com.apple.pasteboard".into()];
        let merged = store.merged_services("unknown", &frontmatter);
        assert_eq!(merged, vec!["a.b.c".to_string()]);
    }
}
