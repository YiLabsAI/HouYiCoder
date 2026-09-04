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
            let mut builder = ignore::gitignore::GitignoreBuilder::new(&self.cwd);
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
    let rel = if p.is_absolute() {
        p.strip_prefix(cwd).ok()?.to_path_buf()
    } else {
        p.to_path_buf()
    };
    let s = rel.to_string_lossy();
    if s.is_empty() || s.starts_with("..") || rel.is_absolute() {
        return None;
    }
    Some(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_api::skill::{SkillDescriptor, SkillError, SkillSnapshot};

    /// Fixed (name, paths) set; list_with_origin returns empty.
    struct StubRegistry {
        skills: Vec<(String, Vec<String>)>,
    }
    impl StubRegistry {
        fn new(skills: &[(&str, &[&str])]) -> Self {
            Self {
                skills: skills
                    .iter()
                    .map(|(n, p)| (n.to_string(), p.iter().map(|s| s.to_string()).collect()))
                    .collect(),
            }
        }
    }
    impl SkillRegistry for StubRegistry {
        fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
            self.skills.iter().map(|(n, _)| descriptor(n)).collect()
        }
        fn find(&self, name: &str) -> Option<SkillDescriptor> {
            self.skills
                .iter()
                .find(|(n, _)| n == name)
                .map(|(n, _)| descriptor(n))
        }
        fn prepare_body(
            &self,
            _name: &str,
            _args: Option<&str>,
            _session_id: Option<&str>,
        ) -> Result<String, SkillError> {
            Ok("body".into())
        }
        fn paths_for(&self, name: &str) -> Vec<String> {
            self.skills
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, p)| p.clone())
                .unwrap_or_default()
        }
    }

    fn descriptor(name: &str) -> SkillDescriptor {
        SkillDescriptor {
            name: name.to_string(),
            description: "stub".to_string(),
            when_to_use: None,
            argument_hint: None,
            disable_model_invocation: false,
            user_invocable: true,
            body_token_estimate: 0,
            allowed_tools: Vec::new(),
            allowed_mach_services: Vec::new(),
            allow_app_launch: false,
        }
    }

    /// Temp dir as the workspace cwd + matcher base.
    fn tmp_cwd() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("houyi-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn test_bare_dir_matches_descendant() {
        // Globstar-strip leaves src; the parent walk lets src match src/foo.rs.
        let reg = Arc::new(StubRegistry::new(&[("web", &["src"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        let activated = act.activate_for_paths(&["src/foo.rs".to_string()]);
        assert_eq!(activated, vec!["web".to_string()]);
        assert!(act.is_active("web"));
    }

    #[test]
    fn test_bare_dir_deep_descendant() {
        let reg = Arc::new(StubRegistry::new(&[("any", &["docs"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        let activated = act.activate_for_paths(&["docs/a/b/c.md".to_string()]);
        assert_eq!(activated, vec!["any".to_string()]);
    }

    /// A real double-star glob matches at any depth.
    #[test]
    fn test_doublestar_glob() {
        let reg = Arc::new(StubRegistry::new(&[("rs", &["**/*.rs"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        let a = act.activate_for_paths(&["a/b/c.rs".to_string()]);
        assert_eq!(a, vec!["rs".to_string()]);
        // a non-matching extension does not activate
        let act2 = ConditionalActivation::new(
            Arc::new(StubRegistry::new(&[("rs", &["**/*.rs"])])),
            tmp_cwd(),
        );
        let b = act2.activate_for_paths(&["a/b/c.md".to_string()]);
        assert!(b.is_empty());
    }

    /// A leading slash anchors the pattern to the workspace root: /src
    /// matches src/foo.rs but not sub/src/foo.rs.
    #[test]
    fn test_leading_slash_anchor() {
        let reg = Arc::new(StubRegistry::new(&[("anchored", &["/src"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        let a = act.activate_for_paths(&["src/foo.rs".to_string()]);
        assert_eq!(a, vec!["anchored".to_string()]);
        let act2 = ConditionalActivation::new(
            Arc::new(StubRegistry::new(&[("anchored", &["/src"])])),
            tmp_cwd(),
        );
        let b = act2.activate_for_paths(&["sub/src/foo.rs".to_string()]);
        assert!(
            b.is_empty(),
            "leading slash anchors to root, no deep match: {b:?}"
        );
    }

    /// The origin map is populated from list_with_origin so the activation
    /// trace can label an untrusted-repo skill.
    #[test]
    fn test_origin_from_registry() {
        struct OriginRegistry;
        impl SkillRegistry for OriginRegistry {
            fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
                vec![descriptor("proj-skill")]
            }
            fn find(&self, name: &str) -> Option<SkillDescriptor> {
                if name == "proj-skill" {
                    Some(descriptor("proj-skill"))
                } else {
                    None
                }
            }
            fn prepare_body(
                &self,
                _name: &str,
                _args: Option<&str>,
                _session_id: Option<&str>,
            ) -> Result<String, SkillError> {
                Ok("body".into())
            }
            fn paths_for(&self, name: &str) -> Vec<String> {
                if name == "proj-skill" {
                    vec!["src".to_string()]
                } else {
                    Vec::new()
                }
            }
            fn list_with_origin(&self) -> Vec<SkillSnapshot> {
                vec![SkillSnapshot {
                    descriptor: descriptor("proj-skill"),
                    origin: "project".to_string(),
                    usage: Default::default(),
                }]
            }
        }
        let act = ConditionalActivation::new(Arc::new(OriginRegistry), tmp_cwd());
        assert_eq!(
            act.origins
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get("proj-skill")
                .map(String::as_str),
            Some("project")
        );
    }

    /// Negation overrides an earlier pattern within the same ordered list,
    /// so bar/x.rs is whitelisted while bar/baz.rs is still ignored.
    #[test]
    fn test_negation_whitelist() {
        let reg = Arc::new(StubRegistry::new(&[("neg", &["bar", "!bar/x.rs"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        let a = act.activate_for_paths(&["bar/x.rs".to_string()]);
        assert!(a.is_empty(), "whitelisted by negation: {a:?}");
        assert!(!act.is_active("neg"));
        let b = act.activate_for_paths(&["bar/baz.rs".to_string()]);
        assert_eq!(b, vec!["neg".to_string()]);
    }

    /// A path escaping cwd via a parent ref must be skipped, not passed to
    /// the matcher (panic guard). The skill stays inactive.
    #[test]
    fn test_outside_cwd_skipped() {
        let reg = Arc::new(StubRegistry::new(&[("out", &["src"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        let a = act.activate_for_paths(&["../outside.rs".to_string()]);
        assert!(a.is_empty());
        assert!(!act.is_active("out"));
    }

    #[test]
    fn test_absolute_outside_cwd_skipped() {
        let reg = Arc::new(StubRegistry::new(&[("abs", &["src"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        let a = act.activate_for_paths(&["/etc/passwd".to_string()]);
        assert!(a.is_empty());
        assert!(!act.is_active("abs"));
    }

    #[test]
    fn test_once_active_stays_active() {
        let reg = Arc::new(StubRegistry::new(&[("once", &["src"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        let a = act.activate_for_paths(&["src/a.rs".to_string()]);
        assert_eq!(a, vec!["once".to_string()]);
        let b = act.activate_for_paths(&["src/b.rs".to_string()]);
        assert!(b.is_empty(), "already-active not re-emitted: {b:?}");
        assert!(act.is_active("once"));
    }

    #[test]
    fn test_empty_conditional_short_circuits() {
        // No skill carries paths; returns empty without touching the matcher.
        let reg = Arc::new(StubRegistry::new(&[("plain", &[])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        assert!(
            act.conditional
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
        let a = act.activate_for_paths(&["src/foo.rs".to_string()]);
        assert!(a.is_empty());
    }

    #[test]
    fn test_non_matching_skipped() {
        let reg = Arc::new(StubRegistry::new(&[("pick", &["src"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        let a = act.activate_for_paths(&["other/x.rs".to_string()]);
        assert!(a.is_empty());
        assert!(!act.is_active("pick"));
        let b = act.activate_for_paths(&["src/foo.rs".to_string()]);
        assert_eq!(b, vec!["pick".to_string()]);
    }

    #[test]
    fn test_origin_map_empty() {
        let reg = Arc::new(StubRegistry::new(&[("x", &["src"])]));
        let act = ConditionalActivation::new(reg, tmp_cwd());
        assert!(
            act.origins
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        );
    }
}
