//! Per-skill sandbox entitlement resolution. Three feeders merge into the
//! final grant: the skill's frontmatter declaration, a compiled capability
//! profile for known community skills, and the user grant store. The Apple
//! deny-list filters all three.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Apple system services never authorizable through any grant path.
pub const DENIED_MACH_SERVICES: &[&str] = &[
    "com.apple.pasteboard",
    "com.apple.cfprefsd",
    "com.apple.coreservices.appleevents",
];

/// A compiled-in mapping of known community skills to the entitlements they
/// need, so a skill works without the user hand-editing the grant store or
/// the vendor adding houyi-specific frontmatter. Parsed from the embedded
/// JSON on first access.
const CAPABILITY_PROFILE_JSON: &str = include_str!("skill-capabilities.json");

fn capability_for(skill: &str) -> Option<(Vec<String>, bool)> {
    static MAP: OnceLock<HashMap<String, serde_json::Value>> = OnceLock::new();
    let map = MAP.get_or_init(|| match serde_json::from_str(CAPABILITY_PROFILE_JSON) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("skill-capabilities profile parse error: {e}");
            HashMap::new()
        }
    });
    let entry = map.get(skill)?;
    let mach: Vec<String> = entry
        .get("mach_services")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let allow_launch = entry
        .get("allow_app_launch")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Some((mach, allow_launch))
}

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

    /// Resolve a skill's sandbox entitlements from all three feeders:
    /// frontmatter, the compiled capability profile, and the user grant
    /// store. Returns the deny-filtered mach-service union and the OR of
    /// every feeder's allow_app_launch flag.
    pub fn resolve(
        &self,
        skill: &str,
        frontmatter: &[String],
        fm_allow_launch: bool,
    ) -> (Vec<String>, bool) {
        let cap = capability_for(skill);
        let mut mach: Vec<String> = frontmatter
            .iter()
            .filter(|s| !DENIED_MACH_SERVICES.contains(&s.as_str()))
            .cloned()
            .collect();
        if let Some((cap_mach, _)) = &cap {
            for s in cap_mach {
                if !DENIED_MACH_SERVICES.contains(&s.as_str()) && !mach.contains(s) {
                    mach.push(s.clone());
                }
            }
        }
        for s in self.grant_for(skill) {
            if !mach.contains(&s) {
                mach.push(s);
            }
        }
        let allow_launch = fm_allow_launch || cap.map(|(_, a)| a).unwrap_or(false);
        (mach, allow_launch)
    }
}

impl Default for SkillGrantStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve entitlements for a skill invocation. When a grant store is
/// wired, delegates to its resolve (three-feeder union). When not wired
/// (tests, no-sandbox), falls back to frontmatter alone — still
/// deny-list filtered, so a skill declaring a system service cannot
/// leak past the fence even without a store.
pub fn resolve_entitlements(
    grants: Option<&SkillGrantStore>,
    skill: &str,
    frontmatter: &[String],
    fm_allow_launch: bool,
) -> (Vec<String>, bool) {
    match grants {
        Some(g) => g.resolve(skill, frontmatter, fm_allow_launch),
        None => {
            let mach: Vec<String> = frontmatter
                .iter()
                .filter(|s| !DENIED_MACH_SERVICES.contains(&s.as_str()))
                .cloned()
                .collect();
            (mach, fm_allow_launch)
        }
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
    fn test_resolve_deny_all_feeders() {
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
        let (mach, allow_launch) = store.resolve("s1", &frontmatter, false);
        // Deny-list entries from all feeders gone.
        assert!(!mach.contains(&"com.apple.pasteboard".to_string()));
        assert!(!mach.contains(&"com.apple.cfprefsd".to_string()));
        // Union of survivors, no dup from overlap.
        assert_eq!(mach, vec!["x.y.z".to_string(), "a.b.c".to_string()]);
        assert!(!allow_launch);
        let _ = std::fs::remove_dir_all(&dir).is_ok();
    }

    #[test]
    fn test_resolve_frontmatter_only() {
        let store = SkillGrantStore {
            grants: Mutex::new(HashMap::new()),
            path: std::env::temp_dir().join("grant-noop.json"),
        };
        let frontmatter = vec!["a.b.c".into(), "com.apple.pasteboard".into()];
        let (mach, allow_launch) = store.resolve("unknown", &frontmatter, false);
        assert_eq!(mach, vec!["a.b.c".to_string()]);
        assert!(!allow_launch);
    }

    #[test]
    fn test_resolve_profile_hit() {
        let store = SkillGrantStore {
            grants: Mutex::new(HashMap::new()),
            path: std::env::temp_dir().join("grant-cap.json"),
        };
        // ego-browser is in the compiled profile; no frontmatter or grant
        // store entry needed.
        let (mach, allow_launch) = store.resolve("ego-browser", &[], false);
        assert!(mach.contains(&"com.citrolabs.ego.lite.ego-browser".to_string()));
        assert!(allow_launch);
    }

    #[test]
    fn test_resolve_none_deny() {
        // None store: frontmatter alone, deny-list still applies.
        let frontmatter = vec!["a.b.c".into(), "com.apple.pasteboard".into()];
        let (mach, allow_launch) = resolve_entitlements(None, "any", &frontmatter, true);
        assert_eq!(mach, vec!["a.b.c".to_string()]);
        assert!(allow_launch);
    }
}
