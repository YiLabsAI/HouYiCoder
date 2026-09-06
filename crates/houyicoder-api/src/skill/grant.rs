//! Per-skill sandbox entitlement resolution. Three feeders merge into the
//! final grant: the skill's frontmatter declaration, a compiled capability
//! profile for known community skills, and the user grant store. The Apple
//! deny-list filters all three.

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Apple services observed during sandbox probes. All services in the
/// com.apple namespace are denied by is_denied; this list records common
/// examples without pretending to be an exhaustive security boundary.
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
    "com.apple.analyticsd",
    "com.apple.dock.server",
    "com.apple.CoreServices.coreservicesd",
    "com.apple.coreservices.quarantine-resolver",
];

/// Whether a mach service belongs to Apple's namespace and must never
/// be granted through any skill-controlled path. Matching is ASCII
/// case-insensitive because Apple service names use inconsistent casing.
pub fn is_denied(service: &str) -> bool {
    const APPLE_PREFIX: &str = "com.apple.";
    service
        .get(..APPLE_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(APPLE_PREFIX))
}

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
const CAPABILITY_PROFILE_JSON: &str = include_str!("../skill-capabilities.json");

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

/// User-owned persistent Mach-service grants, isolated by skill and origin.
pub struct SkillGrantStore {
    grants: Mutex<HashMap<String, Vec<String>>>,
    path: PathBuf,
}

fn grant_path(home: Option<OsString>, user_profile: Option<OsString>) -> io::Result<PathBuf> {
    let home = [home, user_profile]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|path| path.is_absolute())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "absolute user home unavailable",
            )
        })?;
    Ok(home.join(".houyicoder").join("skill-grants.json"))
}

impl SkillGrantStore {
    /// Construct the user-owned grant store. Refuses to initialize when
    /// no home directory is available rather than placing an authority
    /// file inside the model-writable workspace.
    pub fn new() -> io::Result<Self> {
        let path = Self::path()?;
        Ok(Self {
            grants: Mutex::new(load_grants(&path)),
            path,
        })
    }

    /// Construct a store at an explicit host-owned path. Callers must not
    /// choose a location writable by sandboxed commands.
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            grants: Mutex::new(load_grants(&path)),
            path,
        }
    }

    /// Resolve the default grant path from the user's home directory.
    /// Returns an error when neither supported variable is an absolute path.
    pub fn path() -> io::Result<PathBuf> {
        grant_path(env::var_os("HOME"), env::var_os("USERPROFILE"))
    }

    /// Format the composite grant-store key. The key is scoped by both
    /// skill name and origin so a project-level skill with the same name
    /// as a user-level skill cannot consume grants the user approved for
    /// the user-level copy.
    fn grant_key(skill: &str, origin: &str) -> String {
        format!("{skill}\x00{origin}")
    }

    /// Return effective grants for one skill origin after deny filtering.
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

    #[cfg(test)]
    fn set_grant(&self, skill: &str, origin: &str, services: Vec<String>) -> io::Result<()> {
        let filtered: Vec<String> = services.into_iter().filter(|s| !is_denied(s)).collect();
        let key = Self::grant_key(skill, origin);
        let mut grants = self.grants.lock().expect("grant lock poisoned");
        let mut updated = grants.clone();
        updated.insert(key, filtered);
        save_grants(&self.path, &updated)?;
        *grants = updated;
        Ok(())
    }

    /// Persist newly approved services for one skill origin. The complete
    /// read-modify-write is serialized so concurrent approvals cannot lose
    /// updates. Returns only services added by this approval after the write
    /// succeeds; a failed write leaves in-memory grants unchanged.
    pub fn add_grants(
        &self,
        skill: &str,
        origin: &str,
        new_services: Vec<String>,
    ) -> io::Result<Vec<String>> {
        let filtered: Vec<String> = new_services.into_iter().filter(|s| !is_denied(s)).collect();
        let key = Self::grant_key(skill, origin);
        let mut grants = self.grants.lock().expect("grant lock poisoned");
        let existing = grants.get(&key);
        let mut added = Vec::new();
        for service in filtered {
            let already_granted = existing.is_some_and(|items| items.contains(&service));
            if !already_granted && !added.contains(&service) {
                added.push(service);
            }
        }
        if added.is_empty() {
            return Ok(added);
        }
        let mut updated = grants.clone();
        updated
            .entry(key)
            .or_default()
            .extend(added.iter().cloned());
        save_grants(&self.path, &updated)?;
        *grants = updated;
        Ok(added)
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

fn save_grants(path: &Path, grants: &HashMap<String, Vec<String>>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(grants)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let seq = SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp.{pid}.{seq}", pid = process::id()));
    if let Err(e) = fs::write(&tmp, &text) {
        let _cleanup = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _cleanup = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
