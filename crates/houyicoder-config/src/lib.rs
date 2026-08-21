//! The single source of truth for configuration values and
//! environment-variable resolution. Sits at the bottom of the layer cake
//! (alongside the async and resilience foundation leaves) so every crate that needs a base URL, api
//! key, or model id depends on this one place instead of each calling
//! std::env::var inline.
//!
//! Resolution order honours 12-factor (env wins) with project-preferred env
//! var names tried first: DASHSCOPE_ (project .env convention) then OPENAI_
//! (broad convention) then HOUYICODER_ (brand). Defaults are const fallbacks.
//! TOML file cascade lands in a follow-up sprint; the structure is already
//! serde-derived so that step is additive.
//!
//! Test strategy: the priority/default logic lives in pure helpers
//! (first_non_empty, build_provider) that take Options directly, so they are
//! unit-tested without mutating process env (which edition 2024 marks unsafe
//! and this workspace denies). The resolve_* fns are thin env-reading wrappers
//! over those helpers.

pub mod retention;
pub mod sandbox_network;
pub mod settings_store;

pub use sandbox_network::{
    NetworkMode, SandboxNetworkConfig, load_sandbox_network, load_sandbox_network_from,
};
pub use settings_store::{SettingsWriteError, update_settings};
pub mod model_section;
pub use houyicoder_protocol::llm::EffortLevel;
pub use model_section::{ModelEntry, ModelSection, load_model_section_from};
pub mod served_models;
pub use served_models::{ServedModels, cache_path, cached_ids, load_ids_at, served_model_exists};
pub mod api_key;
pub use api_key::api_key_from_helper;
pub mod settings_merge;
pub use settings_merge::{merge_json, read_settings_value};

// ---- defaults (const, no inline literals at call sites) -------------------

/// Default base URL when no env var supplies one: the DashScope
/// compatible-mode endpoint. The project defaults to DashScope; any
/// OpenAI-compatible endpoint overrides via env or settings.json.
pub const DEFAULT_BASE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";

/// Default DashScope compatible-mode endpoint, used by live integration tests
/// and the project .env when DASHSCOPE_BASE_URL is unset.
pub const DEFAULT_DASHSCOPE_BASE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";

/// Default model id when settings.json has no model.id. Picked for the
/// project default DashScope account tier (verified working); any model id
/// the account can access overrides via the catalog (settings.json).
pub const DEFAULT_MODEL: &str = "qwen3.7-max";

// ---- env var names --------------------------------------------------------

pub const ENV_DASHSCOPE_API_KEY: &str = "DASHSCOPE_API_KEY";
pub const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
pub const ENV_HOUYICODER_API_KEY: &str = "HOUYICODER_API_KEY";
pub const ENV_DASHSCOPE_BASE_URL: &str = "DASHSCOPE_BASE_URL";
pub const ENV_OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";

/// Env var for the list of external tool servers to spawn at startup. The
/// value is a JSON array of objects with program and args fields. An empty
/// or unset value means no external server is wired. Example:
/// HOUYICODER_MCP_SERVERS='[{"program":"node","args":["server.js"]}]'
pub const ENV_HOUYICODER_MCP_SERVERS: &str = "HOUYICODER_MCP_SERVERS";

/// Env var for the list of external command hooks to register at startup. The
/// value is a JSON array of objects with name, events (a list of event names
/// such as PreToolUse or PostToolUse), program, and args fields. An empty or
/// unset value means no command hook is wired. Example:
/// HOUYICODER_HOOKS='[{"name":"lint","events":["PreToolUse"],"program":"sh","args":["-c","..."]}]'
pub const ENV_HOUYICODER_HOOKS: &str = "HOUYICODER_HOOKS";

// ---- types ----------------------------------------------------------------

/// Resolved provider configuration: the three values a caller needs to
/// construct a provider. serde-derived for the future TOML cascade.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Configuration resolution error. Kept narrow; providers map these to their
/// own error enums (e.g. MissingApiKey -> ProviderError::Auth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// None of the recognised api-key env vars were set. Fail-closed: callers
    /// should not send anonymous requests.
    MissingApiKey,
}

/// A non-fatal problem found while loading settings: one field was
/// malformed so it fell back to its default, but the rest of the file
/// was still read. Surfaced to the user so a typo does not silently
/// become a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    pub field: String,
    pub reason: String,
}

/// Pull a boolean field out of a parsed settings Value, defaulting to true
/// and recording a warning when the field is missing or the wrong type.
/// Per-field recovery (rather than failing the whole file) is the point: a
/// single bad value must not silently reset every other setting.
fn extract_bool_field(
    value: &serde_json::Value,
    field: &str,
    warnings: &mut Vec<ConfigWarning>,
) -> bool {
    match value.get(field) {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(other) => {
            warnings.push(ConfigWarning {
                field: field.to_string(),
                reason: format!(
                    "expected a boolean, got {}; using the default (true)",
                    json_type_name(other)
                ),
            });
            true
        }
    }
}

/// One-word JSON value type name, for warning wording.
pub(crate) fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// One external tool server to spawn at startup. The composition root reads
/// the list from env, spawns each via the launcher, and wraps its tools
/// behind the Tool trait. The first cut wires one server; the shape is a Vec
/// so a multi-server config lands without a schema change.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfig {
    /// The program to run (a binary name or path).
    pub program: String,
    /// Argv after the program.
    pub args: Vec<String>,
}

/// One external command hook. The composition root spawns the program per
/// fire, pipes the hook context as JSON to stdin, and parses the verdict JSON
/// from stdout. Events are strings resolved against the runtime HookEvent
/// enum at the composition root, so this leaf crate stays free of any
/// dependency on the agent layer. A spec with an empty name or program is
/// dropped by the parser, mirroring the tool-server config: a typo must not
/// silently register a no-op hook.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HookSpec {
    /// A stable label the hook identifies itself by in verdicts and logs.
    pub name: String,
    /// Event names (PreToolUse, PostToolUse, PostToolUseFailure, ...). An
    /// unknown name skips this hook at registration, not at fire time.
    pub events: Vec<String>,
    /// The program to run (a binary name or path).
    pub program: String,
    /// Argv after the program.
    #[serde(default)]
    pub args: Vec<String>,
}

// ---- pure helpers (logic under test, no env mutation) ---------------------

/// The first non-empty value in priority order, or None. Empty strings are
/// treated as unset so a stray empty env var does not shadow a later one.
fn first_non_empty(vals: &[Option<String>]) -> Option<String> {
    vals.iter()
        .find_map(|v| v.as_ref().filter(|s| !s.is_empty()).cloned())
}

/// Assemble a ProviderConfig from already-resolved pieces; MissingApiKey when
/// no key is available. Separated from load_provider so the assembly logic is
/// testable without touching process env.
fn build_provider(
    api_key: Option<String>,
    base_url: String,
    model: String,
) -> Result<ProviderConfig, ConfigError> {
    let api_key = api_key.ok_or(ConfigError::MissingApiKey)?;
    Ok(ProviderConfig {
        base_url,
        api_key,
        model,
    })
}

// ---- resolution (thin env-reading wrappers) --------------------------------

/// The first available api key, or None. Resolution chain: the apiKeyHelper
/// script in settings.json (the key stays out of the file), then the env
/// vars in project-preferred order. The helper result is cached for the
/// process (the script runs once at startup); env is read fresh each call.
pub fn resolve_api_key() -> Option<String> {
    static HELPER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let helper = HELPER.get_or_init(|| api_key_from_helper(&settings_path()));
    helper.clone().or_else(|| {
        first_non_empty(&[
            std::env::var(ENV_DASHSCOPE_API_KEY).ok(),
            std::env::var(ENV_OPENAI_API_KEY).ok(),
            std::env::var(ENV_HOUYICODER_API_KEY).ok(),
        ])
    })
}

/// The base URL to point a provider at. DASHSCOPE_BASE_URL wins, then
/// OPENAI_BASE_URL, then DEFAULT_BASE_URL. Never fails (always has a default).
pub fn resolve_base_url() -> String {
    first_non_empty(&[
        std::env::var(ENV_DASHSCOPE_BASE_URL).ok(),
        std::env::var(ENV_OPENAI_BASE_URL).ok(),
    ])
    .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// The model id to send in CompletionRequest.model. Resolution chain:
/// settings.json model.id → DEFAULT_MODEL. Reads no process env so a stray
/// env var cannot shadow a persisted pick (the test-knob-as-authority bug);
/// the test harness injects a temp settings path instead. The per-session
/// sidecar override sits above this layer at the composition root (resume
/// reads session meta.model, then falls back here). Never fails.
pub fn resolve_model() -> String {
    resolve_model_from(&settings_path())
}

/// Pure loader against an explicit path; testable without env mutation. Chain:
/// settings.json model.id → DEFAULT_MODEL. A missing file, a corrupt file, or
/// a blank id all yield DEFAULT_MODEL — the load_model_section_from helper
/// already degrades per-field with warnings, so this layer only picks the id.
pub fn resolve_model_from(path: &std::path::Path) -> String {
    let (section, _warnings) = load_model_section_from(path);
    match section
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => id.to_string(),
        None => DEFAULT_MODEL.to_string(),
    }
}

/// Load a fully-resolved ProviderConfig, or MissingApiKey when no key env var
/// is set (fail-closed). base_url and model always resolve via their defaults.
pub fn load_provider() -> Result<ProviderConfig, ConfigError> {
    build_provider(resolve_api_key(), resolve_base_url(), resolve_model())
}

/// The list of external tool servers configured via env. An empty or unset var
/// yields an empty Vec so the composition root wires no external server (the
/// built-in tools still load). A malformed value is also an empty Vec with a
/// stderr warning — the engine must not brick on a config typo.
pub fn resolve_mcp_servers() -> Vec<McpServerConfig> {
    let raw = std::env::var(ENV_HOUYICODER_MCP_SERVERS).ok();
    match parse_mcp_servers(raw.as_deref()) {
        Ok(list) => list,
        Err(msg) => {
            tracing::warn!(
                "{ENV_HOUYICODER_MCP_SERVERS} ignored ({msg}); no external tool server wired"
            );
            Vec::new()
        }
    }
}

/// Pure parser for the external server list; testable without env mutation.
/// Accepts a JSON array of objects with program and args fields. An empty or
/// unset value yields an empty list. A non-array value is an error.
fn parse_mcp_servers(raw: Option<&str>) -> Result<Vec<McpServerConfig>, String> {
    let Some(body) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    let parsed: Vec<McpServerConfig> =
        serde_json::from_str(body).map_err(|e| format!("invalid json: {e}"))?;
    // Drop entries with an empty program: a server with no command cannot
    // spawn, and silently registering zero tools from it would mask the
    // config typo.
    Ok(parsed
        .into_iter()
        .filter(|c| !c.program.is_empty())
        .collect())
}

/// The list of external command hooks configured via env. An empty or unset
/// var yields an empty Vec so the composition root wires no command hook (the
/// engine still runs, the built-in fire points still fire, just no external
/// verdict source). A malformed value is also an empty Vec with a stderr
/// warning — the engine must not brick on a config typo.
pub fn resolve_hooks() -> Vec<HookSpec> {
    let raw = std::env::var(ENV_HOUYICODER_HOOKS).ok();
    match parse_hooks(raw.as_deref()) {
        Ok(list) => list,
        Err(msg) => {
            tracing::warn!("{ENV_HOUYICODER_HOOKS} ignored ({msg}); no command hooks wired");
            Vec::new()
        }
    }
}

/// Pure parser for the command hook list; testable without env mutation.
/// Accepts a JSON array of objects with name, events, program, and args
/// fields. An empty or unset value yields an empty list. A non-array value
/// is an error. Entries with an empty name or program are dropped: a hook
/// with no command cannot spawn, and silently registering a no-op hook
/// would mask the config typo.
fn parse_hooks(raw: Option<&str>) -> Result<Vec<HookSpec>, String> {
    let Some(body) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    let parsed: Vec<HookSpec> =
        serde_json::from_str(body).map_err(|e| format!("invalid json: {e}"))?;
    Ok(parsed
        .into_iter()
        .filter(|s| !s.name.is_empty() && !s.program.is_empty())
        .collect())
}

// ---- memory toggles (cross-session, file-backed) -------------------------

/// The config-home directory under the user HOME. Falls back to the OS
/// temp dir when HOME is unset (tests / headless CI) so load/save never
/// panic — a missing home just means the settings file lives in a temp
/// root and the defaults apply on the next load if it cannot be written.
/// HOUYICODER_CONFIG_HOME overrides the HOME-derived root (tests isolate
/// the config dir from the developer's real home without env races on
/// HOME itself).
pub fn config_home() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("HOUYICODER_CONFIG_HOME") {
        return std::path::PathBuf::from(dir);
    }
    let base = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join(".houyicoder")
}

/// The settings file path inside the config-home directory.
pub fn settings_path() -> std::path::PathBuf {
    config_home().join("settings.json")
}

/// User-tunable memory feature switches, persisted across sessions in the
/// settings file. Both default to on (auto-memory ships enabled by default);
/// a user who wants a quieter session turns them off and the choice sticks.
/// The runner gates recall injection on auto_memory and the background
/// consolidation dream on auto_dream.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryToggles {
    /// When false, the runner skips turn-entry recall injection and the
    /// background extractor. Existing memories still surface via the store.
    #[serde(default = "default_true")]
    pub auto_memory: bool,
    /// When false, the background consolidation dream never fires. The
    /// extractor still runs (it is the cheaper, deterministic half).
    #[serde(default = "default_true")]
    pub auto_dream: bool,
}

fn default_true() -> bool {
    true
}

impl Default for MemoryToggles {
    fn default() -> Self {
        Self {
            auto_memory: true,
            auto_dream: true,
        }
    }
}

/// Load the toggles from the settings file. A missing or corrupt file yields
/// the default (both on) — the toggles are advisory UX, never a hard gate
/// that bricks the session on a malformed file.
pub fn load_toggles() -> (MemoryToggles, Vec<ConfigWarning>) {
    load_toggles_from(&settings_path())
}

/// Pure loader against an explicit path; testable without env mutation. A
/// missing file yields defaults with no warnings. A corrupt file yields
/// defaults plus a single warning. A valid file with one bad field yields
/// that field's default plus a warning, but the other fields are read
/// normally — one typo does not reset the whole file.
pub fn load_toggles_from(path: &std::path::Path) -> (MemoryToggles, Vec<ConfigWarning>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return (MemoryToggles::default(), Vec::new()),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return (
                MemoryToggles::default(),
                vec![ConfigWarning {
                    field: "<file>".into(),
                    reason: "settings.json is not valid JSON; using defaults".into(),
                }],
            );
        }
    };
    let mut warnings = Vec::new();
    let auto_memory = extract_bool_field(&value, "auto_memory", &mut warnings);
    let auto_dream = extract_bool_field(&value, "auto_dream", &mut warnings);
    (
        MemoryToggles {
            auto_memory,
            auto_dream,
        },
        warnings,
    )
}

/// Persist the toggles to the settings file best-effort (atomic temp+rename).
/// A write failure is logged and ignored — the in-memory toggle still takes
/// effect for the session; only cross-session persistence is lost.
pub fn save_toggles(toggles: &MemoryToggles) {
    save_toggles_to(&settings_path(), toggles);
}

/// Pure saver against an explicit path; testable without env mutation.
/// Delegates to update_settings so the write is merge-preserving (other keys
/// like sandbox.network survive) and CAS-guarded. Best-effort: a write failure
/// is dropped here — the caller still has the in-memory state. Surfacing the
/// error to the UI is a follow-up once a runtime warning channel exists.
pub fn save_toggles_to(path: &std::path::Path, toggles: &MemoryToggles) {
    let auto_memory = toggles.auto_memory;
    let auto_dream = toggles.auto_dream;
    drop(update_settings(
        path,
        |v| {
            v["auto_memory"] = auto_memory.into();
            v["auto_dream"] = auto_dream.into();
        },
        3,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_section_round_trip() {
        let path = std::env::temp_dir().join(format!("model-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"model":{"id":"qwen3-coder","effort_level":"high","catalog":[{"id":"qwen3-coder","effort":"low","context_window":131072}]}}"#,
        )
        .unwrap();
        let (s, w) = load_model_section_from(&path);
        assert_eq!(s.id.as_deref(), Some("qwen3-coder"));
        assert_eq!(s.effort_level, Some(EffortLevel::High));
        assert_eq!(s.catalog.len(), 1);
        assert_eq!(s.catalog[0].id, "qwen3-coder");
        assert_eq!(s.catalog[0].effort, Some(EffortLevel::Low));
        assert_eq!(s.catalog[0].context_window, Some(131072));
        assert!(w.is_empty(), "no warnings on a valid model section");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_model_section_missing() {
        let path = std::env::temp_dir().join(format!("model-missing-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"auto_memory":true}"#).unwrap();
        let (s, w) = load_model_section_from(&path);
        assert!(s.id.is_none(), "no model section => default");
        assert!(!s.catalog.is_empty(), "falls back to default catalog");
        assert!(w.is_empty(), "a missing section is not a warning");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_model_section_corrupt() {
        let path = std::env::temp_dir().join(format!("model-corrupt-{}.json", std::process::id()));
        std::fs::write(&path, "not json {").unwrap();
        let (s, w) = load_model_section_from(&path);
        assert!(s.id.is_none(), "corrupt => default");
        assert_eq!(w.len(), 1, "corrupt file warns");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_defaults_are_set() {
        assert_eq!(
            DEFAULT_BASE_URL,
            "https://dashscope.aliyuncs.com/compatible-mode/v1"
        );
        assert_eq!(DEFAULT_MODEL, "qwen3.7-max");
    }

    #[test]
    fn test_first_non_empty_priority() {
        // First non-empty wins; order encodes the env var priority chain.
        assert_eq!(
            first_non_empty(&[Some("a".into()), Some("b".into())]).as_deref(),
            Some("a")
        );
        assert_eq!(
            first_non_empty(&[None, Some("b".into()), None]).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn test_first_non_empty_skips() {
        // A set-but-empty env var must not shadow a later real value.
        assert_eq!(
            first_non_empty(&[Some(String::new()), Some("b".into())]).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn test_first_non_empty_all() {
        assert_eq!(first_non_empty(&[None, None, None]), None);
        assert_eq!(
            first_non_empty(&[Some(String::new()), Some(String::new())]),
            None
        );
    }

    #[test]
    fn test_build_provider_missing_key() {
        assert_eq!(
            build_provider(None, DEFAULT_BASE_URL.into(), DEFAULT_MODEL.into()).unwrap_err(),
            ConfigError::MissingApiKey
        );
    }

    #[test]
    fn test_build_provider_full() {
        let cfg = build_provider(Some("k".into()), "https://b/v1".into(), "m".into()).unwrap();
        assert_eq!(cfg.api_key, "k");
        assert_eq!(cfg.base_url, "https://b/v1");
        assert_eq!(cfg.model, "m");
    }

    #[test]
    fn test_resolve_base_url_default() {
        // When neither env var is set in the test process, the default applies.
        // (make check does not source .env, so these are unset here.)
        let base = resolve_base_url();
        assert!(
            base == DEFAULT_BASE_URL
                || base == std::env::var(ENV_DASHSCOPE_BASE_URL).unwrap_or_default()
                || base == std::env::var(ENV_OPENAI_BASE_URL).unwrap_or_default()
        );
    }

    #[test]
    fn test_resolve_model_default() {
        // A settings file with no model.id yields DEFAULT_MODEL. Uses the
        // pure loader against a temp path so the test never touches the real
        // user settings.json, and no env var can shadow the pick.
        let path = std::env::temp_dir().join(format!("no-model-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"auto_memory":true}"#).unwrap();
        assert_eq!(resolve_model_from(&path), DEFAULT_MODEL);
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_resolve_model_from_id() {
        // A settings.json with model.id overrides DEFAULT_MODEL. The env
        // layer is gone, so the persisted id is the authority.
        let path = std::env::temp_dir().join(format!("with-model-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
        )
        .unwrap();
        assert_eq!(resolve_model_from(&path), "qwen3-coder");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_resolve_model_ignores_env() {
        // Setting HOUYICODER_TEST_MODEL in the process must not shadow the
        // settings pick: resolve_* reads no env. The var name still exists in
        // the wild; resolving ignores it (whether set or unset).
        let path = std::env::temp_dir().join(format!("no-env-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
        )
        .unwrap();
        assert_eq!(resolve_model_from(&path), "qwen3-coder");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn test_parse_mcp_servers_empty() {
        // An unset or empty value yields no external server.
        assert!(parse_mcp_servers(None).unwrap().is_empty());
        assert!(parse_mcp_servers(Some("")).unwrap().is_empty());
        assert!(parse_mcp_servers(Some("   ")).unwrap().is_empty());
    }

    #[test]
    fn test_parse_mcp_servers_array() {
        let raw = r#"[
            {"program":"node","args":["server.js"]},
            {"program":"python","args":["-m","srv"]}
        ]"#;
        let list = parse_mcp_servers(Some(raw)).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].program, "node");
        assert_eq!(list[0].args, vec!["server.js"]);
        assert_eq!(list[1].program, "python");
        assert_eq!(list[1].args, vec!["-m", "srv"]);
    }

    #[test]
    fn test_parse_mcp_servers_invalid() {
        // A non-array value is an error so a config typo does not silently
        // register zero tools.
        assert!(parse_mcp_servers(Some("not json")).is_err());
        assert!(parse_mcp_servers(Some("[{}]")).is_err());
    }

    #[test]
    fn test_parse_servers_drops_empty() {
        let raw = r#"[{"program":"","args":[]}]"#;
        let list = parse_mcp_servers(Some(raw)).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_parse_hooks_empty() {
        assert!(parse_hooks(None).unwrap().is_empty());
        assert!(parse_hooks(Some("")).unwrap().is_empty());
        assert!(parse_hooks(Some("   ")).unwrap().is_empty());
    }

    #[test]
    fn test_parse_hooks_array() {
        let raw = r#"[{"name":"lint","events":["PreToolUse"],"program":"sh","args":["-c","echo"]},{"name":"log","events":["PostToolUse"],"program":"cat"}]"#;
        let list = parse_hooks(Some(raw)).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "lint");
        assert_eq!(list[0].events, vec!["PreToolUse"]);
        assert_eq!(list[0].args, vec!["-c", "echo"]);
        assert_eq!(list[1].name, "log");
        assert!(list[1].args.is_empty());
    }

    #[test]
    fn test_parse_hooks_invalid() {
        assert!(parse_hooks(Some("not json")).is_err());
        // A hook missing the required name field fails to deserialize.
        assert!(parse_hooks(Some(r#"[{"events":[],"program":"sh"}]"#)).is_err());
    }

    #[test]
    fn test_parse_hooks_round_trips() {
        let spec = HookSpec {
            name: "lint".into(),
            events: vec!["PreToolUse".into()],
            program: "sh".into(),
            args: vec!["-c".into(), "echo hi".into()],
        };
        let s = serde_json::to_string(&spec).unwrap();
        let back: HookSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn test_parse_hooks_drops_empty() {
        let raw = r#"[{"name":"","events":["PreToolUse"],"program":"sh"},{"name":"x","events":[],"program":""}]"#;
        let list = parse_hooks(Some(raw)).unwrap();
        assert!(list.is_empty());
    }

    /// A missing settings file yields the default (both on).
    #[test]
    fn test_load_missing_is_default() {
        let path = std::env::temp_dir().join(format!("nope-{}.json", std::process::id()));
        let (t, _w) = load_toggles_from(&path);
        assert!(t.auto_memory && t.auto_dream, "missing file => both on");
    }

    /// A corrupt settings file yields the default, not a panic.
    #[test]
    fn test_load_corrupt_is_default() {
        let path = std::env::temp_dir().join(format!("bad-{}.json", std::process::id()));
        std::fs::write(&path, "not json {").unwrap();
        let (t, _w) = load_toggles_from(&path);
        assert!(t.auto_memory && t.auto_dream, "corrupt file => both on");
        drop(std::fs::remove_file(&path));
    }

    /// Save then load round-trips a non-default state.
    #[test]
    fn test_save_load_round_trips() {
        let path = std::env::temp_dir().join(format!("toggles-{}.json", std::process::id()));
        let toggles = MemoryToggles {
            auto_memory: false,
            auto_dream: true,
        };
        save_toggles_to(&path, &toggles);
        let (loaded, _w) = load_toggles_from(&path);
        assert_eq!(loaded, toggles, "round-trip preserves the off/on state");
        drop(std::fs::remove_file(&path));
    }

    /// config_home + settings_path resolve under the user HOME. Deterministic
    /// (no file I/O, just path construction from HOME which the test env sets).
    #[test]
    fn test_config_home_resolves() {
        assert!(config_home().ends_with(".houyicoder"));
        let path = settings_path();
        assert!(path.ends_with("settings.json"));
        assert!(path.starts_with(config_home()));
    }

    /// A settings JSON missing a field uses the serde default, so a partial
    /// file from an older build does not brick the load.
    #[test]
    fn test_partial_json_uses_defaults() {
        let path = std::env::temp_dir().join(format!("partial-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"auto_dream":false}"#).unwrap();
        let (t, _w) = load_toggles_from(&path);
        assert!(t.auto_memory, "missing auto_memory defaults on");
        assert!(!t.auto_dream, "auto_dream false from the file");
        drop(std::fs::remove_file(&path));
    }

    /// A bad-typed field falls back to its default and warns; a sibling field
    /// is still read normally. One typo does not reset the whole file.
    #[test]
    fn test_bad_field_defaults_warns() {
        let path = std::env::temp_dir().join(format!("badfield-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"auto_memory":"yes","auto_dream":false}"#).unwrap();
        let (t, warnings) = load_toggles_from(&path);
        assert!(t.auto_memory, "bad auto_memory falls back to default true");
        assert!(
            !t.auto_dream,
            "auto_dream read normally despite sibling bad field"
        );
        assert_eq!(warnings.len(), 1, "one warning for the bad field");
        assert!(
            warnings[0].field.contains("auto_memory"),
            "warning names the bad field"
        );
        drop(std::fs::remove_file(&path));
    }
}
