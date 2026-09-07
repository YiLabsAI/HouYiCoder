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

use super::{GrantScope, GrantSubject, SkillSource};

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

/// User-owned persistent Mach-service grants, isolated by typed authority subject.
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

    fn grant_key(subject: &GrantSubject) -> String {
        let scope = match &subject.scope {
            GrantScope::Managed => "managed".to_string(),
            GrantScope::UserHome => "user_home".to_string(),
            GrantScope::Project(identity) => format!("project:{}", identity.as_str()),
            GrantScope::Remote(identity) => format!("remote:{}", identity.0),
        };
        format!("v2\x00{}\x00{scope}", subject.skill)
    }

    /// Return effective grants for one typed subject after deny filtering.
    pub fn grant_for(&self, subject: &GrantSubject) -> Vec<String> {
        let key = Self::grant_key(subject);
        self.grants
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !is_denied(s))
            .collect()
    }

    #[cfg(test)]
    fn set_grant(&self, subject: &GrantSubject, services: Vec<String>) -> io::Result<()> {
        let filtered: Vec<String> = services.into_iter().filter(|s| !is_denied(s)).collect();
        let key = Self::grant_key(subject);
        let mut grants = self
            .grants
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut updated = grants.clone();
        updated.insert(key, filtered);
        save_grants(&self.path, &updated)?;
        *grants = updated;
        Ok(())
    }

    /// Persist newly approved services for one skill authority. The complete
    /// read-modify-write is serialized so concurrent approvals cannot lose
    /// updates. Returns only services added by this approval after the write
    /// succeeds; a failed write leaves in-memory grants unchanged.
    pub fn add_grants(
        &self,
        subject: &GrantSubject,
        new_services: Vec<String>,
    ) -> io::Result<Vec<String>> {
        let filtered: Vec<String> = new_services.into_iter().filter(|s| !is_denied(s)).collect();
        let key = Self::grant_key(subject);
        let mut grants = self
            .grants
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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
    /// Host-owned profiles apply to managed and user-home skills. Skill-authored
    /// frontmatter applies only to managed and native user-home skills. Explicit
    /// grants are isolated by user, project-root, or remote-provider authority.
    pub fn resolve(
        &self,
        skill: &str,
        source: &SkillSource,
        frontmatter: &[String],
        fm_allow_launch: bool,
    ) -> (Vec<String>, bool) {
        resolve_feeders(
            skill,
            source,
            frontmatter,
            fm_allow_launch,
            self.grant_for(&source.grant_subject(skill)),
        )
    }
}

fn resolve_feeders(
    skill: &str,
    source: &SkillSource,
    frontmatter: &[String],
    fm_allow_launch: bool,
    stored: Vec<String>,
) -> (Vec<String>, bool) {
    let cap = source
        .applies_capability_profile()
        .then(|| capability_for(skill))
        .flatten();
    let mut mach: Vec<String> = if source.applies_frontmatter_entitlements() {
        frontmatter
            .iter()
            .filter(|service| !is_denied(service))
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    if let Some((cap_mach, _)) = &cap {
        for service in cap_mach {
            if !is_denied(service) && !mach.contains(service) {
                mach.push(service.clone());
            }
        }
    }
    for service in stored {
        if !mach.contains(&service) {
            mach.push(service);
        }
    }
    let frontmatter_launch = source.applies_frontmatter_entitlements() && fm_allow_launch;
    let profile_launch = cap.map(|(_, allow_launch)| allow_launch).unwrap_or(false);
    (mach, frontmatter_launch || profile_launch)
}

/// Resolve entitlements for a skill invocation. When a grant store is
/// wired, delegates to its resolve (three-feeder union). When not wired
/// (tests, no-sandbox), resolves the host profile and permitted frontmatter
/// without persistent user grants. A missing typed source fails closed.
pub fn resolve_entitlements(
    grants: Option<&SkillGrantStore>,
    skill: &str,
    source: Option<&SkillSource>,
    frontmatter: &[String],
    fm_allow_launch: bool,
) -> (Vec<String>, bool) {
    let Some(source) = source else {
        return (Vec::new(), false);
    };
    match grants {
        Some(grants) => grants.resolve(skill, source, frontmatter, fm_allow_launch),
        None => resolve_feeders(skill, source, frontmatter, fm_allow_launch, Vec::new()),
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
