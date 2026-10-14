//! Tests for this module.

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
    let dir = temp_dir().join(format!("houyi-ca-{}", id()));
    create_dir_all(&dir).ok();
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

/// An absolute path under cwd strips to a relative path and matches.
#[test]
fn test_absolute_under_cwd_matches() {
    let cwd = tmp_cwd();
    let reg = Arc::new(StubRegistry::new(&[("abs", &["src"])]));
    let act = ConditionalActivation::new(reg, cwd.clone());
    let abs = cwd.join("src/foo.rs").to_string_lossy().to_string();
    let a = act.activate_for_paths(&[abs]);
    assert_eq!(a, vec!["abs".to_string()]);
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
