//! Per-skill sandbox entitlement resolution. Three feeders merge into the
//! final grant: the skill's frontmatter declaration, a compiled capability
//! profile for known community skills, and the user grant store. The Apple
//! deny-list filters all three.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Apple system services never authorizable through any grant path.
/// Matched by prefix so suffixed runtime variants (e.g.
/// com.apple.pasteboard.1) are also denied. Intentionally short — the
/// hard floor, not the only screening layer.
pub const DENIED_MACH_SERVICES: &[&str] = &[
    "com.apple.pasteboard",
    "com.apple.cfprefsd",
    "com.apple.coreservices.appleevents",
    "com.apple.tccd",
    "com.apple.lsd",
    "com.apple.SecurityServer",
    "com.apple.securityd",
    "com.apple.coreservices.launchservicesd",
    "com.apple.contactsd",
    "com.apple.DiskArbitration.diskarbitrationd",
    "com.apple.logd",
    "com.apple.system.notification_center",
    "com.apple.system.opendirectoryd",
    "com.apple.windowserver.active",
    "com.apple.distributed_notifications",
];

/// Whether a mach service name is on the Apple deny-list and must never
/// be granted through any path. Matches a listed root exactly or any
/// suffixed variant (root followed by a dot and the runtime suffix), so
/// com.apple.pasteboard.1 is denied even though only the root
/// com.apple.pasteboard is listed.
pub fn is_denied(service: &str) -> bool {
    DENIED_MACH_SERVICES
        .iter()
        .any(|root| service == *root || service.starts_with(&format!("{root}.")))
}

/// Discovery origins whose frontmatter and compiled-profile entitlements
/// are trusted to install sandbox capabilities directly. Managed and
/// user-level sources (user, agents, claude_eco, local) are machine-local
/// and user-installed — the user chose to put them there. Project and
/// Whether a skill origin may install entitlements directly from
/// frontmatter or the compiled profile. Converged to the same set as
/// body trust (managed + user): every other origin — including
/// agents, claude_eco, local, project, and mcp — must go through
/// deny-log discovery and explicit approval. A repo that ships
/// .claude/skills or .agents/skills gets claude_eco/agents origin,
/// which is untrusted for entitlements even though the body may be
/// served; the capability direction is the more dangerous one, so
/// it gets the stricter gate.
pub fn is_entitlement_trusted_origin(origin: &str) -> bool {
    origin == "managed" || origin == "user"
}

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

    /// Construct a store backed by an explicit path. For tests that must
    /// not touch the real user home directory.
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            grants: Mutex::new(load_grants(&path)),
            path,
        }
    }

    pub fn path() -> PathBuf {
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".houyicoder").join("skill-grants.json")
    }

    /// Format the composite grant-store key. The key is scoped by both
    /// skill name and origin so a project-level skill with the same name
    /// as a user-level skill cannot consume grants the user approved for
    /// the user-level copy.
    fn grant_key(skill: &str, origin: &str) -> String {
        format!("{skill}\x00{origin}")
    }

    pub fn grant_for(&self, skill: &str, origin: &str) -> Vec<String> {
        let key = Self::grant_key(skill, origin);
        self.grants
            .lock()
            .expect("grant lock poisoned")
            .get(&key)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !is_denied(s))
            .collect()
    }

    pub fn set_grant(&self, skill: &str, origin: &str, services: Vec<String>) {
        let filtered: Vec<String> = services.into_iter().filter(|s| !is_denied(s)).collect();
        let key = Self::grant_key(skill, origin);
        let mut grants = self.grants.lock().expect("grant lock poisoned");
        grants.insert(key, filtered);
        save_grants(&self.path, &grants);
    }

    /// Merge new services into an existing skill+origin grant atomically:
    /// the read and the write are serialized inside one lock hold so two
    /// concurrent approvals for the same skill cannot lose an update.
    /// Deny-listed services are filtered before merge. Returns the
    /// merged set so the caller can report what was granted.
    pub fn add_grants(&self, skill: &str, origin: &str, new_services: Vec<String>) -> Vec<String> {
        let filtered: Vec<String> = new_services.into_iter().filter(|s| !is_denied(s)).collect();
        let key = Self::grant_key(skill, origin);
        let mut grants = self.grants.lock().expect("grant lock poisoned");
        let existing = grants.entry(key).or_default();
        for s in &filtered {
            if !existing.contains(s) {
                existing.push(s.clone());
            }
        }
        let merged = existing.clone();
        save_grants(&self.path, &grants);
        merged
    }

    /// Resolve a skill's sandbox entitlements from up to three feeders:
    /// frontmatter, the compiled capability profile, and the user grant
    /// store. Returns the deny-filtered mach-service union and the OR of
    /// every feeder's allow_app_launch flag.
    ///
    /// When trusted is false (non-managed/user origin), frontmatter and
    /// the compiled capability profile are skipped — a repo-checked-in or
    /// server-sourced skill cannot install sandbox capabilities by
    /// declaring them in its own frontmatter or by matching a known
    /// skill's name. The grant store is keyed by (skill, origin) so a
    /// project-level skill with the same name as a user-level skill
    /// cannot consume grants the user approved for the user-level copy.
    pub fn resolve(
        &self,
        skill: &str,
        origin: &str,
        frontmatter: &[String],
        fm_allow_launch: bool,
        trusted: bool,
    ) -> (Vec<String>, bool) {
        let cap = if trusted { capability_for(skill) } else { None };
        let fm_mach: Vec<String> = if trusted {
            frontmatter
                .iter()
                .filter(|s| !is_denied(s))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let fm_launch = if trusted { fm_allow_launch } else { false };
        let mut mach = fm_mach;
        if let Some((cap_mach, _)) = &cap {
            for s in cap_mach {
                if !is_denied(s) && !mach.contains(s) {
                    mach.push(s.clone());
                }
            }
        }
        for s in self.grant_for(skill, origin) {
            if !mach.contains(&s) {
                mach.push(s);
            }
        }
        let allow_launch = fm_launch || cap.map(|(_, a)| a).unwrap_or(false);
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
/// leak past the fence even without a store. When trusted is false
/// (project or mcp origin), frontmatter is ignored and only the grant
/// store feeds in — untrusted sources must go through deny-log
/// discovery and explicit approval.
pub fn resolve_entitlements(
    grants: Option<&SkillGrantStore>,
    skill: &str,
    origin: &str,
    frontmatter: &[String],
    fm_allow_launch: bool,
    trusted: bool,
) -> (Vec<String>, bool) {
    match grants {
        Some(g) => g.resolve(skill, origin, frontmatter, fm_allow_launch, trusted),
        None => {
            if !trusted {
                return (Vec::new(), false);
            }
            let mach: Vec<String> = frontmatter
                .iter()
                .filter(|s| !is_denied(s))
                .cloned()
                .collect();
            (mach, fm_allow_launch)
        }
    }
}

fn load_grants(path: &Path) -> HashMap<String, Vec<String>> {
    let Ok(text) = fs::read_to_string(path) else {
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

/// Per-writer temp-file counter so two concurrent saves do not truncate
/// each other's temp file (a shared name means one writer can rename a
/// file holding the other's half-written bytes).
static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

fn save_grants(path: &Path, grants: &HashMap<String, Vec<String>>) {
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        tracing::warn!("skill-grants create dir failed: {e}");
        return;
    }
    let text = match serde_json::to_string_pretty(grants) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("skill-grants serialize failed: {e}");
            return;
        }
    };
    let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{pid}.{seq}", pid = process::id()));
    if let Err(e) = fs::write(&tmp, &text) {
        tracing::warn!("skill-grants tmp write failed: {e}");
        let _ = fs::remove_file(&tmp).is_ok();
        return;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        tracing::warn!("skill-grants rename failed: {e}");
        let _ = fs::remove_file(&tmp).is_ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp directory + skill-grants.json path for one test.
    /// Uses pid + atomic counter so parallel tests in the same process
    /// do not collide, and never touches the real user home directory.
    fn fresh_grant_path() -> PathBuf {
        use std::sync::atomic::AtomicU32;
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("houyi-skill-grant-{}-{n}", process::id()));
        let _ = fs::remove_dir_all(&dir).is_ok();
        fs::create_dir_all(&dir).expect("mkdir test dir");
        dir.join("skill-grants.json")
    }

    /// A store backed by a fresh temp path. For tests that need set_grant
    /// to persist to disk without touching the real user home.
    fn fresh_store() -> SkillGrantStore {
        let path = fresh_grant_path();
        SkillGrantStore {
            grants: Mutex::new(HashMap::new()),
            path,
        }
    }

    /// A store backed by a fresh temp path pre-loaded from a specific
    /// file content (for corrupt-file tests etc.).
    fn fresh_store_with_content(content: &str) -> SkillGrantStore {
        let path = fresh_grant_path();
        fs::write(&path, content).expect("write test content");
        SkillGrantStore {
            grants: Mutex::new(load_grants(&path)),
            path,
        }
    }

    #[test]
    fn test_grant_unknown_empty() {
        let store = fresh_store();
        assert!(store.grant_for("nope", "user").is_empty());
    }

    #[test]
    fn test_disk_round_trip() {
        let path = fresh_grant_path();
        {
            let store = SkillGrantStore::with_path(path.clone());
            store.set_grant("ego-browser", "user", vec!["a.b.c".into()]);
        }
        let reloaded = SkillGrantStore::with_path(path.clone());
        assert_eq!(
            reloaded.grant_for("ego-browser", "user"),
            vec!["a.b.c".to_string()]
        );
        let _ = fs::remove_dir_all(path.parent().unwrap()).is_ok();
    }

    #[test]
    fn test_set_and_read() {
        let store = fresh_store();
        store.set_grant("ego-browser", "user", vec!["a.b.c".into()]);
        assert_eq!(
            store.grant_for("ego-browser", "user"),
            vec!["a.b.c".to_string()]
        );
    }

    /// The composite (skill, origin) key prevents a same-name untrusted
    /// skill from consuming grants approved for a trusted/user copy.
    /// A grant set under origin "user" must not be readable under
    /// origin "project" — the central security property of origin-aware
    /// grant keying.
    #[test]
    fn test_cross_origin_grant_isolation() {
        let store = fresh_store();
        store.set_grant("ego-browser", "user", vec!["a.b.c".into()]);
        assert!(
            store.grant_for("ego-browser", "project").is_empty(),
            "project origin must not see user-origin grants"
        );
        assert!(
            store.grant_for("ego-browser", "agents").is_empty(),
            "agents origin must not see user-origin grants"
        );
        assert_eq!(
            store.grant_for("ego-browser", "user"),
            vec!["a.b.c".to_string()],
            "user origin must still see its own grants"
        );
    }

    #[test]
    fn test_denied_filtered() {
        let store = fresh_store();
        // Real macOS log names are suffixed: pasteboard.1, cfprefsd.daemon.
        // The deny-list carries only the roots; the filter must still drop
        // the suffixed variants, or a discovered com.apple.pasteboard.1
        // would ride through to the approval card.
        store.set_grant(
            "s1",
            "user",
            vec![
                "com.apple.pasteboard.1".into(),
                "com.apple.cfprefsd.daemon".into(),
                "a.b.c".into(),
            ],
        );
        let granted = store.grant_for("s1", "user");
        assert!(!granted.contains(&"com.apple.pasteboard.1".to_string()));
        assert!(!granted.contains(&"com.apple.cfprefsd.daemon".to_string()));
        assert!(granted.contains(&"a.b.c".to_string()));
    }

    #[test]
    fn test_is_denied_prefix_match() {
        // Roots match exactly.
        assert!(is_denied("com.apple.pasteboard"));
        assert!(is_denied("com.apple.tccd"));
        // Suffixed runtime variants match by prefix.
        assert!(is_denied("com.apple.pasteboard.1"));
        assert!(is_denied("com.apple.cfprefsd.daemon"));
        assert!(is_denied("com.apple.securityd.session"));
        assert!(is_denied("com.apple.coreservices.launchservicesd.agent"));
        // A nested suffix is still a variant of the root.
        assert!(is_denied("com.apple.pasteboard.foo.bar"));
    }

    #[test]
    fn test_is_denied_dot_boundary() {
        // A name that shares the root as a substring but is not a suffixed
        // variant is not denied — the dot boundary separates a real
        // variant from an unrelated name that happens to start the same.
        assert!(!is_denied("com.apple.pasteboardx"));
        assert!(!is_denied("com.apple.tccdx"));
        // A non-Apple service is never denied.
        assert!(!is_denied("com.citrolabs.ego.lite.ego-browser"));
        assert!(!is_denied("com.houyi.test.entitlement"));
    }

    #[test]
    fn test_corrupt_file_returns_empty() {
        let store = fresh_store_with_content("not valid json {{{");
        assert!(store.grant_for("any", "user").is_empty());
    }

    #[test]
    fn test_resolve_deny_all_feeders() {
        let store = fresh_store();
        // Grant store side filtered (pasteboard.1 is a suffixed variant of
        // a denied root; the filter must drop it, not just the bare root).
        store.set_grant(
            "s1",
            "user",
            vec!["com.apple.pasteboard.1".into(), "x.y.z".into()],
        );
        // Frontmatter side with a deny-listed service + an overlap entry.
        let frontmatter = vec![
            "com.apple.cfprefsd.daemon".into(),
            "x.y.z".into(),
            "a.b.c".into(),
        ];
        let (mach, allow_launch) = store.resolve("s1", "user", &frontmatter, false, true);
        // Deny-list entries from all feeders gone, including suffixed.
        assert!(!mach.contains(&"com.apple.pasteboard.1".to_string()));
        assert!(!mach.contains(&"com.apple.cfprefsd.daemon".to_string()));
        // Union of survivors, no dup from overlap.
        assert_eq!(mach, vec!["x.y.z".to_string(), "a.b.c".to_string()]);
        assert!(!allow_launch);
    }

    #[test]
    fn test_resolve_frontmatter_only() {
        let store = fresh_store();
        // Suffixed variant of a denied root must be filtered here too.
        let frontmatter = vec!["a.b.c".into(), "com.apple.pasteboard.1".into()];
        let (mach, allow_launch) = store.resolve("unknown", "user", &frontmatter, false, true);
        assert_eq!(mach, vec!["a.b.c".to_string()]);
        assert!(!allow_launch);
    }

    #[test]
    fn test_resolve_profile_hit() {
        let store = fresh_store();
        // ego-browser is in the compiled profile; no frontmatter or grant
        // store entry needed. The profile grants app launch (ego-browser
        // starts the ego lite app via LaunchServices) and the bootstrap
        // mach service the ego lite process connects to.
        let (mach, allow_launch) = store.resolve("ego-browser", "user", &[], false, true);
        assert_eq!(
            mach,
            vec!["com.citrolabs.ego.lite.ego-browser".to_string()],
            "ego-browser profile declares its bootstrap mach service"
        );
        assert!(allow_launch, "ego-browser profile grants app launch");
    }

    #[test]
    fn test_resolve_none_deny() {
        // None store: frontmatter alone, deny-list still applies to
        // suffixed variants.
        let frontmatter = vec!["a.b.c".into(), "com.apple.pasteboard.1".into()];
        let (mach, allow_launch) =
            resolve_entitlements(None, "any", "user", &frontmatter, true, true);
        assert_eq!(mach, vec!["a.b.c".to_string()]);
        assert!(allow_launch);
    }

    #[test]
    fn test_resolve_untrusted_skips_declarations() {
        // A project-sourced skill naming itself ego-browser must NOT get
        // the compiled profile (allow_app_launch: true, mach services)
        // and must NOT get its own frontmatter declarations. Only the
        // grant store feeds in — and only with services the user already
        // explicitly approved.
        let store = fresh_store();
        // Pre-seed the grant store as if the user had approved one service
        // for this skill through the entitlement card.
        store.set_grant("ego-browser", "user", vec!["user.approved.svc".into()]);
        // Frontmatter declares a system service + a regular service, and
        // requests app launch. All of these must be ignored.
        let frontmatter = vec![
            "com.apple.pasteboard.1".into(),
            "attacker.declared.svc".into(),
        ];
        let (mach, allow_launch) = store.resolve("ego-browser", "user", &frontmatter, true, false);
        // Only the user-approved grant store service survives.
        assert_eq!(mach, vec!["user.approved.svc".to_string()]);
        assert!(!allow_launch, "untrusted source must not get app launch");
    }

    #[test]
    fn test_resolve_untrusted_no_store() {
        // Untrusted + no store: nothing at all, even with frontmatter
        // declaring services and app launch.
        let frontmatter = vec!["a.b.c".into(), "d.e.f".into()];
        let (mach, allow_launch) =
            resolve_entitlements(None, "any", "user", &frontmatter, true, false);
        assert!(mach.is_empty());
        assert!(!allow_launch);
    }

    #[test]
    fn test_entitlement_trust_boundary() {
        // Only managed and user origins are trusted for entitlements.
        assert!(is_entitlement_trusted_origin("managed"));
        assert!(is_entitlement_trusted_origin("user"));
        // All other origins — including agents, claude_eco, local —
        // are untrusted: a repo can ship .claude/skills or
        // .agents/skills, so these must go through deny-log discovery.
        assert!(!is_entitlement_trusted_origin("agents"));
        assert!(!is_entitlement_trusted_origin("claude_eco"));
        assert!(!is_entitlement_trusted_origin("local"));
        assert!(!is_entitlement_trusted_origin("project"));
        assert!(!is_entitlement_trusted_origin("mcp"));
        // Unknown origin fails closed.
        assert!(!is_entitlement_trusted_origin(""));
        assert!(!is_entitlement_trusted_origin("unknown"));
    }
}
