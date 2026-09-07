//! The skill registry port: the engine-facing contract for discovering
//! skills and preparing their bodies. The concrete implementation (which
//! reads SKILL.md files from disk) lives in the composition root; the
//! engine depends on this trait so it does not depend on the skill data
//! crate directly. Object-safe (sync methods) so the engine holds
//! Arc<dyn SkillRegistry> and the concrete registry swaps behind it.
//!
//! Methods are synchronous: the discovered set is cached at startup, and
//! body preparation is a one-shot file read plus string substitution.
//! The Skill tool wraps them inside its async execute.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::path::Path;

use sha2::{Digest, Sha256};

pub mod grant;

/// The directory or transport family that supplied a skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillFamily {
    /// The native skill family.
    Houyi,
    /// A compatible ecosystem directory family.
    ClaudeEco,
    /// The interoperable agents directory family.
    Agents,
    /// A remote prompt family.
    Mcp,
}

/// Non-reversible identity for a canonical project root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectIdentity(String);

impl ProjectIdentity {
    /// Hash a canonical project root into a stable machine-local identity.
    pub fn from_canonical_root(root: &Path) -> Self {
        let mut hasher = Sha256::new();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            hasher.update(root.as_os_str().as_bytes());
        }
        #[cfg(windows)]
        {
            let case_folded = root.to_string_lossy().to_lowercase();
            hasher.update(case_folded.as_bytes());
        }
        let digest = hasher.finalize();
        let mut encoded = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(encoded, "{byte:02x}");
        }
        Self(encoded)
    }

    /// Return the encoded project-root identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable identity for a remote skill provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteIdentity(
    /// Provider identity assigned by the host connection.
    pub String,
);

/// The authority boundary where a skill was discovered.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SkillProvenance {
    /// Host-managed installation.
    Managed,
    /// Installation below the user's home directory.
    UserHome,
    /// Installation associated with a stable project identity.
    Project(ProjectIdentity),
    /// Installation supplied by a remote server identity.
    Remote(RemoteIdentity),
}

/// A skill's orthogonal family and authority provenance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SkillSource {
    /// Directory or transport compatibility family.
    pub family: SkillFamily,
    /// Host-derived authority boundary.
    pub provenance: SkillProvenance,
}

impl SkillSource {
    /// Construct a typed source from its independent dimensions.
    pub fn new(family: SkillFamily, provenance: SkillProvenance) -> Self {
        Self { family, provenance }
    }

    /// Whether the skill body and hooks are trusted host instructions.
    pub fn is_trusted(&self) -> bool {
        matches!(
            self.provenance,
            SkillProvenance::Managed | SkillProvenance::UserHome
        )
    }

    /// Whether the host-owned compiled capability profile applies.
    pub fn applies_capability_profile(&self) -> bool {
        self.is_trusted()
    }

    /// Whether skill-authored frontmatter may directly grant entitlements.
    pub fn applies_frontmatter_entitlements(&self) -> bool {
        matches!(self.provenance, SkillProvenance::Managed)
            || matches!(self.provenance, SkillProvenance::UserHome)
                && self.family == SkillFamily::Houyi
    }

    /// Build the stable authority subject used by the persistent grant store.
    pub fn grant_subject(&self, skill: &str) -> GrantSubject {
        let scope = match &self.provenance {
            SkillProvenance::Managed => GrantScope::Managed,
            SkillProvenance::UserHome => GrantScope::UserHome,
            SkillProvenance::Project(identity) => GrantScope::Project(identity.clone()),
            SkillProvenance::Remote(identity) => GrantScope::Remote(identity.clone()),
        };
        GrantSubject {
            skill: skill.to_string(),
            scope,
        }
    }
}

/// Stable authority scope for a persisted entitlement grant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GrantScope {
    /// Host-managed scope.
    Managed,
    /// Shared user-home scope across local directory families.
    UserHome,
    /// One canonical project root.
    Project(ProjectIdentity),
    /// One remote server identity.
    Remote(RemoteIdentity),
}

/// Typed persistent-grant identity, independent of display origin labels.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrantSubject {
    /// Skill directory identity.
    pub skill: String,
    /// Stable authority scope.
    pub scope: GrantScope,
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

impl GrantSubject {
    /// Encode the subject for a host-generated synthetic approval payload.
    pub fn to_json(&self) -> serde_json::Value {
        let (kind, identity) = match &self.scope {
            GrantScope::Managed => ("managed", None),
            GrantScope::UserHome => ("user_home", None),
            GrantScope::Project(identity) => ("project", Some(identity.0.as_str())),
            GrantScope::Remote(identity) => ("remote", Some(identity.0.as_str())),
        };
        serde_json::json!({
            "skill": self.skill,
            "kind": kind,
            "identity": identity,
        })
    }

    /// Decode and validate a subject carried by a synthetic approval payload.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let skill = value.get("skill")?.as_str()?;
        if !valid_skill_name(skill) {
            return None;
        }
        let scope = match value.get("kind")?.as_str()? {
            "managed" => GrantScope::Managed,
            "user_home" => GrantScope::UserHome,
            "project" => {
                let identity = value.get("identity")?.as_str()?;
                if identity.len() != 64 || !identity.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return None;
                }
                GrantScope::Project(ProjectIdentity(identity.to_ascii_lowercase()))
            }
            "remote" => {
                let identity = value.get("identity")?.as_str()?;
                if identity.is_empty() || identity.len() > 256 || identity.contains('\0') {
                    return None;
                }
                GrantScope::Remote(RemoteIdentity(identity.to_string()))
            }
            _ => return None,
        };
        Some(Self {
            skill: skill.to_string(),
            scope,
        })
    }
}

/// A minimal, engine-facing view of a discovered skill. Carries only the
/// fields the engine consumes (listing, invocation gating, cost visibility);
/// the full parsed definition stays in the skill data crate and never
/// crosses this port.
#[derive(Debug, Clone)]
pub struct SkillDescriptor {
    /// Identity (directory name). Matches ^[a-z0-9-]+$.
    pub name: String,
    /// One-line description for the model-visible listing.
    pub description: String,
    /// Optional "when to use" guidance appended to the listing entry.
    pub when_to_use: Option<String>,
    /// Optional argument hint shown in the slash palette.
    pub argument_hint: Option<String>,
    /// True when the skill is hidden from the model-visible listing and
    /// blocked from Skill-tool invocation (frontmatter
    /// disable-model-invocation). User slash dispatch is unaffected.
    pub disable_model_invocation: bool,
    /// True when the user cannot invoke the skill via slash (frontmatter
    /// user-invocable: false). Model invocation is unaffected.
    pub user_invocable: bool,
    /// Rough body token estimate (bytes / 4) so the model and the host
    /// see the invocation cost before committing.
    pub body_token_estimate: u32,
    /// Additive tool grants from frontmatter allowed-tools. Non-empty means
    /// the skill requests permission-bearing properties, so the Skill tool
    /// asks before executing (the safe-property allowlist gate).
    pub allowed_tools: Vec<String>,
    /// macOS XPC service names the skill needs the sandbox to allow
    /// (frontmatter allowed-mach-services). Emitted as extra
    /// allow mach-lookup lines in the seatbelt profile so the sandboxed
    /// process can talk to macOS system services the base set excludes.
    /// Empty on non-macOS or when the skill declares none.
    pub allowed_mach_services: Vec<String>,
    /// Whether the skill needs to launch apps via open -a (frontmatter
    /// allow-app-launch). Grants the LaunchServices entitlement so a
    /// sandboxed command can start an app; false by default.
    pub allow_app_launch: bool,
}

/// Errors a skill registry can return when preparing a body.
#[derive(Debug, Clone)]
pub enum SkillError {
    /// No skill with the given name was found in the discovered set.
    NotFound(String),
    /// The body file could not be read (missing, permission, io).
    BodyLoad(String),
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillError::NotFound(name) => write!(f, "skill not found: {name}"),
            SkillError::BodyLoad(msg) => write!(f, "skill body load failed: {msg}"),
        }
    }
}

impl Error for SkillError {}

/// The engine-facing skill registry. The Skill tool + the slash dispatch
/// both call find (to gate on their own invocation flag) then prepare_body
/// (the shared body-prep, ungated — the two paths converge there). The
/// turn-entry listing step calls list_model_invocable. The concrete
/// implementation wraps the skill data crate (discovery + body
/// preparation) and is constructed at the composition root.
pub trait SkillRegistry: Send + Sync {
    /// Skills visible to the model (disable-model-invocation filtered out),
    /// in precedence order. Used to build the per-turn listing attachment.
    fn list_model_invocable(&self) -> Vec<SkillDescriptor>;

    /// Look up a skill by name. The caller checks the invocation flag
    /// (disable-model-invocation for the Skill tool, user-invocable for
    /// slash) before preparing the body — gating is the caller's job,
    /// the registry only resolves + describes. None when no skill
    /// matches.
    fn find(&self, name: &str) -> Option<SkillDescriptor>;

    /// Load + prepare the body for a named skill: read the body file,
    /// strip frontmatter, prepend the base-dir header, substitute
    /// arguments and variables. Ungated — the caller gates on the
    /// invocation flag via find. Returns NotFound when no skill matches,
    /// BodyLoad when the body file could not be read. The session id
    /// feeds variable substitution; None when the dispatch is not
    /// session-bound.
    fn prepare_body(
        &self,
        name: &str,
        args: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<String, SkillError>;

    /// All discovered skills paired with their discovery origin (managed /
    /// user / project / claude_eco / agents / mcp / local), for surfaces that
    /// group skills by source — the /skills pane. Unlike
    /// list_model_invocable, this does NOT filter disable-model-invocation
    /// skills: the visibility surface shows them (marked not invocable) so
    /// the user can see they are blocked from the model. The origin is
    /// carried alongside the descriptor (rather than on the descriptor) so
    /// SkillDescriptor stays under the field-count warn line. The default
    /// returns empty: a registry that does not track origin reports no
    /// grouped skills, so production registries override this.
    fn list_with_origin(&self) -> Vec<SkillSnapshot> {
        Vec::new()
    }

    /// Return the host-derived typed source for one discovered skill. None
    /// fails closed when a registry cannot supply provenance.
    fn source_for(&self, _name: &str) -> Option<SkillSource> {
        None
    }

    /// Detect skill-directory script executions in a Bash command. Returns one
    /// entry per script the command runs from a discovered skill's directory,
    /// carrying the skill + relative script path for the approval card. The
    /// default returns empty: a registry that does not track skill directories
    /// reports no scripts, so the existing protected-path ask still surfaces
    /// but without the script path.
    fn detect_run_scripts(&self, _command: &str) -> Vec<SkillScriptRef> {
        Vec::new()
    }

    /// Parsed frontmatter hooks for a skill, deep-parsed at discovery
    /// (safeParse: malformed hooks drop the hooks, not the skill). Each
    /// spec carries the event, matcher, command + args, once flag, if-rule,
    /// and the hook-source level the registry gates by (MCP skills produce
    /// no specs — they never register). The default returns empty.
    fn hooks_for(&self, _name: &str) -> Vec<SkillHookSpec> {
        Vec::new()
    }

    /// Normalized frontmatter paths for a skill (gitignore-style globs the
    /// skill activates on when a touched file matches). Empty means the
    /// skill is unconditional (always visible to the model). The conditional
    /// activation is session-scoped: the listing step filters these out
    /// until a file touch matches, then they stay visible for the session.
    /// Kept off SkillDescriptor so the descriptor stays under the field-count
    /// warn line, mirroring hooks_for. The default returns empty.
    fn paths_for(&self, _name: &str) -> Vec<String> {
        Vec::new()
    }

    /// Record a skill invocation attempt. refused=true for a gate refusal
    /// (user-invocable, conditional, disable-model-invocation, load error);
    /// refused=false for a successful body preparation. NotASkill and
    /// NotFound are not recorded (an unknown skill is not an invocation
    /// of a known one). The default is no-op: a registry that does not
    /// track usage silently drops the record, so tests must exercise the
    /// concrete impl to verify recording.
    fn record_invocation(&self, _name: &str, _refused: bool) {}

    /// The per-skill usage stats accumulated this session. Default empty
    /// when the registry does not track usage. Used by list_with_origin
    /// to pair each snapshot with its usage.
    fn usage_for(&self, _name: &str) -> SkillUsage {
        SkillUsage::default()
    }

    /// Set the session-scoped disabled skill names. A disabled skill is
    /// excluded from the model listing (the model cannot see or invoke it)
    /// but stays visible in the /skills pane (marked disabled). The default
    /// is no-op: a registry that does not track session state silently
    /// drops the set. Called by the server before a run starts, from the
    /// TUI's skill_disabled state.
    fn set_session_disabled(&self, _disabled: HashSet<String>) {}
}

/// A model-invocable descriptor paired with where it was discovered, for
/// source-grouped surfaces. See list_with_origin.
#[derive(Debug, Clone)]
pub struct SkillSnapshot {
    pub descriptor: SkillDescriptor,
    /// Backward-compatible display origin. This never carries a project root.
    pub origin: String,
    /// Session-scoped invocation stats for this skill. Default (zeros)
    /// when the registry does not track usage.
    pub usage: SkillUsage,
}

/// Per-skill invocation stats accumulated within a session. invocations
/// counts successful body preparations; refusals counts gate refusals
/// (user-invocable, conditional, disable-model-invocation, load error).
/// last_used_secs is the epoch-seconds timestamp of the most recent
/// invocation (0 when never invoked). These are NOT ok/fail metrics for
/// skill quality — the true outcome lives downstream in the turn that
/// consumed the body, which this layer cannot see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillUsage {
    pub invocations: u64,
    pub refusals: u64,
    pub last_used_secs: u64,
}

/// A skill-directory script a Bash command runs, surfaced for the per-script
/// confirmation card. The approval prompt shows the skill + relative script
/// path so the user can see what would execute before approving. The path is
/// verifiable (the user can read the file); a first-line summary is NOT shown,
/// because it is attacker-controlled text the card would frame as an
/// authoritative summary. The detection is a heuristic over the command string
/// and the discovered skill directories; a deliberately obfuscated command
/// can evade it, but the sandbox fence remains the hard floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillScriptRef {
    /// The skill whose directory the script lives under.
    pub skill_name: String,
    /// Path relative to the skill directory, e.g. "scripts/deploy.py".
    pub script_rel_path: String,
}

/// The trust level a skill-sourced hook registers under, mirroring the
/// registry's HookSource. MCP skills produce no specs (filtered at parse),
/// so this carries no remote variant. Managed/User are trusted; Project
/// and Local gate on the current workspace trust at registration time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookSourceKind {
    Managed,
    User,
    Project,
    Local,
}

/// One parsed frontmatter hook, ready for the registry to build a command
/// hook from. The event is a string (the port cannot depend on the
/// engine's HookEvent enum); the engine maps it at registration. The
/// matcher and if-rule filter at fire time, not registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHookSpec {
    /// Lifecycle event name (e.g. "PreToolUse"); the engine maps it to HookEvent.
    pub event: String,
    /// Tool-name pattern (exact, A|B, or regex); None fires for every tool.
    pub matcher: Option<String>,
    /// Program to spawn for the hook.
    pub command: String,
    /// Args to the program; command + args form the spawn vector.
    pub args: Vec<String>,
    /// Self-remove after the first successful fire.
    pub once: bool,
    /// Tool-input permission-rule (e.g. "Bash(git *)"); None is unconditional.
    pub if_rule: Option<String>,
    /// Trust level the registry gates the hook by.
    pub source: HookSourceKind,
}
