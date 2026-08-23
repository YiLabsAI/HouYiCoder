//! Durable permission rules. A rule pairs a tool name with an optional content
//! pattern and an effect (allow / deny / ask). Last-match-wins across the rule
//! set, but deny wins: any matching Deny blocks the call regardless of later
//! matches. A rule overrides the mode default policy.
//!
//! Content matching is three-tier: exact (Bash(npm install)), prefix
//! (Bash(npm:*)), and glob (Bash(git *)). A rule with no content matches the tool regardless of input;
//! a rule with content matches only when the tool's input string satisfies the
//! pattern. The input string is the primary field of the call: the command for
//! shell tools, the path for file tools, the url for fetch tools.

use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};

use crate::mode::ModeError;
use crate::store::Scope;

/// The effect a rule assigns to a matched tool action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    Allow,
    Deny,
    Ask,
}

impl Effect {
    pub fn parse(s: &str) -> Result<Self, ModeError> {
        match s.to_ascii_lowercase().as_str() {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            "ask" => Ok(Self::Ask),
            other => Err(ModeError(format!("unknown effect: {other}"))),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

/// A case-insensitive glob pattern. Wraps glob::Pattern so an invalid pattern
/// is rejected at construction, not at every match.
#[derive(Debug, Clone)]
pub struct GlobPattern(Pattern);

impl GlobPattern {
    pub fn new(pat: &str) -> Result<Self, ModeError> {
        Pattern::new(pat)
            .map(Self)
            .map_err(|e| ModeError(format!("bad glob {pat:?}: {e}")))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn matches(&self, name: &str) -> bool {
        self.0.matches_with(name, MATCH_OPTS)
    }
}

const MATCH_OPTS: MatchOptions = MatchOptions {
    case_sensitive: false,
    require_literal_separator: false,
    require_literal_leading_dot: false,
};

/// A content pattern matched against the tool's input string. The three
/// tiers: exact, prefix, and glob. Prefix is the form behind the colon-star
/// syntax (npm:* matches npm and the rest after the space); glob covers
/// free-form patterns (git *).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleContent {
    /// Literal full-string match: the input must equal this string exactly.
    Exact(String),
    /// Prefix match: the input must start with this string. Models the
    /// Tool(prefix:*) form, where the prefix is everything before the star.
    Prefix(String),
    /// Glob match: a glob pattern matched case-insensitively against the
    /// input. Stored as the source string and compiled on demand; an invalid
    /// pattern never matches (fail-closed).
    Glob(String),
}

impl RuleContent {
    /// Parse a content spec into a tier. A trailing colon-star (foo:*) is a
    /// prefix rule; a spec containing glob metacharacters is a glob rule;
    /// otherwise it is an exact match. This is the inverse of the
    /// Tool(content) shorthand read from settings.
    pub fn parse(spec: &str) -> Self {
        if let Some(prefix) = spec.strip_suffix(":*") {
            return Self::Prefix(prefix.to_string());
        }
        if spec.contains('*') || spec.contains('?') || spec.contains('[') {
            return Self::Glob(spec.to_string());
        }
        Self::Exact(spec.to_string())
    }

    /// Whether an input string satisfies this content pattern.
    pub fn matches(&self, input: &str) -> bool {
        match self {
            Self::Exact(s) => input == s,
            Self::Prefix(s) => input.starts_with(s),
            Self::Glob(s) => GlobPattern::new(s)
                .map(|g| g.matches(input))
                .unwrap_or(false),
        }
    }
}

/// A durable permission rule: a tool name (action) plus an optional content
/// pattern, an effect, and the persistence scope (destination) the rule lands
/// in. Last-match-wins; deny wins across the set. A rule with no content
/// matches the tool regardless of input; a rule with content matches only when
/// the input string satisfies the pattern. The scope is a persistence concern
/// only — the evaluator reads the union across scopes; it never inspects
/// scope at decision time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub action: String,
    pub content: Option<RuleContent>,
    pub effect: Effect,
    /// Where the rule persists. Defaults to project (repo-shared) so a rule
    /// constructed by name-only callers lands in the default write scope.
    #[serde(default)]
    pub scope: Scope,
}

impl Rule {
    /// A tool-level rule (no content): matches the tool regardless of input.
    /// This is the backward-compatible constructor for callers that add rules
    /// by tool name only. Scope defaults to project.
    pub fn new(action: &str, effect: Effect) -> Result<Self, ModeError> {
        if action.trim().is_empty() {
            return Err(ModeError("empty tool name".into()));
        }
        Ok(Self {
            action: action.to_string(),
            content: None,
            effect,
            scope: Scope::default(),
        })
    }

    /// A content-scoped rule: matches the tool only when the input string
    /// satisfies the content pattern. Scope defaults to project.
    pub fn with_content(
        action: &str,
        content: RuleContent,
        effect: Effect,
    ) -> Result<Self, ModeError> {
        if action.trim().is_empty() {
            return Err(ModeError("empty tool name".into()));
        }
        Ok(Self {
            action: action.to_string(),
            content: Some(content),
            effect,
            scope: Scope::default(),
        })
    }

    /// Set the persistence scope (destination) the rule lands in. Builder;
    /// returns the rule for chaining.
    pub fn with_scope(mut self, scope: Scope) -> Self {
        self.scope = scope;
        self
    }
}

/// Whether two rules are the same durable directive (dedup judgment
/// point). Action is compared case-insensitively (matching evaluate's
/// action comparison); content + effect + scope are compared
/// structurally. Centralizing the judgment on Rule means the store
/// (add/remove) and the gate's in-memory dedup all share one identity
/// point, and a future canonical normalization (e.g. Glob star to None
/// for tool-wide) changes one method, not scattered inline comparisons.
impl Rule {
    pub fn same_as(&self, other: &Rule) -> bool {
        self.action.eq_ignore_ascii_case(&other.action)
            && self.content == other.content
            && self.effect == other.effect
            && self.scope == other.scope
    }
}

/// Derive the prefix to scope a bash always-allow rule to, mirroring the
/// reference yes-prefix-edited path (bashToolUseOptions.tsx:342-357), which
/// NEVER writes a content-less rule. A content-less Rule::new(tool, Allow)
/// matches every future bash call — after one always-allow on npm install,
/// "rm -rf /" would auto-run. Scoping to a command prefix closes that
/// privilege-escalation footgun.
///
/// Returns None (refuse to persist) only when a prefix cannot be derived:
/// - compound command (A && B) — a single prefix rule cannot scope it;
/// - non-attestable segment (pipes, redirects) the gate cannot reason about.
///
/// Destructive first tokens (rm/sudo/...) ARE allowed to persist a prefix
/// rule — a user may always-allow rm in default mode (the rule writes
/// rm:*); the dangerous-rule stripper removes it in classifier mode.
/// Protected paths (.git, .claude) are still denied regardless of any rule
/// by the bypass-immune safety layer. The earlier "not eligible" refusal
/// refused to persist and confused users.
///
/// Returns Some("npm run") for "npm run build", Some("ls") for "ls -la",
/// Some("rm") for "rm -rf /tmp/build".
pub fn bash_always_allow_prefix(command: &str) -> Option<String> {
    let segs = crate::compound::split_compound(command);
    if segs.len() != 1 {
        return None;
    }
    let cmd = segs[0].trim();
    if !crate::compound::is_attestable(cmd) {
        return None;
    }
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let first = tokens.first().copied()?;
    if RUNNERS.contains(&first) && tokens.len() >= 2 {
        // Scope to runner+subcommand (e.g. "npm run") — tighter than the
        // runner name alone.
        return Some(format!("{first} {}", tokens[1]));
    }
    Some(first.to_string())
}

/// The four history-checkpoint subcommands that ship as builtin ask rules:
/// each is recoverable via reflog (not destructive) and local (not egress),
/// so they would silently Allow in Auto without a rule. A human checkpoint
/// before commit / rebase / reset / tag is the intent — the user sees the
/// op before history moves. git push is NOT here: a plain push is not a
/// checkpoint (the force-push discard form stays a detection validator, not
/// a rule, because it needs argument semantics a glob cannot express).
pub(crate) const BUILTIN_GIT_CHECKPOINTS: &[&str] = &["commit", "rebase", "reset", "tag"];

/// The builtin seed rules: one ask rule per git history-checkpoint
/// subcommand, scoped so the prefix matches that subcommand only. Seeded
/// into the rule set at construction and never written to disk; the user
/// shadows one by adding a rule of their own in any writable scope. The
/// ids are stable so a disabled-builtin config list can target them by id
/// across releases even when the rule wording evolves.
pub fn builtin_rules() -> Vec<Rule> {
    BUILTIN_GIT_CHECKPOINTS
        .iter()
        .map(|sub| {
            // The content is the "git <sub>" prefix: matches any bash call
            // whose command starts with "git <sub>".
            let content = RuleContent::Prefix(format!("git {sub}"));
            Rule {
                action: "bash".to_string(),
                content: Some(content),
                effect: Effect::Ask,
                scope: Scope::Builtin,
            }
        })
        .collect()
}

/// The stable id of a builtin rule, for the disabled-builtin config list.
/// Tracks the rule's content prefix so the id is human-readable in the
/// config file (bash(git commit:*) etc.). A builtin rule whose id is in the
/// disabled list is not seeded.
pub fn builtin_rule_id(rule: &Rule) -> Option<String> {
    if rule.scope != Scope::Builtin {
        return None;
    }
    let content = rule.content.as_ref()?;
    Some(format!("{}({})", rule.action, content_id(content)))
}

fn content_id(content: &RuleContent) -> String {
    match content {
        RuleContent::Exact(s) => s.clone(),
        RuleContent::Prefix(s) => format!("{s}:*"),
        RuleContent::Glob(s) => s.clone(),
    }
}

/// Runners whose first subcommand is the meaningful scope unit (npm run,
/// cargo build) — a compound prefix is tighter than just the runner name.
const RUNNERS: &[&str] = &[
    "npm", "pnpm", "yarn", "npx", "cargo", "go", "deno", "python", "python3", "pip", "pip3",
    "make", "docker", "kubectl", "gh", "brew", "git", "rustup", "uv", "poetry",
];

/// Extract the primary content string from a tool call's JSON input. For shell
/// tools this is the command; for file tools the path; for fetch tools the
/// url. Unknown tools fall back to the serialized JSON. None input yields the
/// empty string, so a content-scoped rule never matches a call with no input
/// (tool-name-only rules still apply).
pub fn input_content(tool_name: &str, input: Option<&serde_json::Value>) -> String {
    let Some(v) = input else {
        return String::new();
    };
    let key = match tool_name.to_ascii_lowercase().as_str() {
        "bash" | "sh" | "exec" | "shell" => "command",
        "write" | "edit" | "multiedit" | "patch" | "str_replace" => "path",
        "webfetch" | "netfetch" | "fetch" | "curl" | "wget" => "url",
        _ => return v.to_string(),
    };
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

/// Evaluate the rule set against a tool name and its input string. Returns the
/// effect of the last matching rule, unless any matching rule denies (deny
/// wins). A rule matches when its action equals the request tool
/// (case-insensitive) and, if it has content, the content pattern matches the
/// input string. None when no rule matches (the caller falls back to the mode
/// default policy).
pub fn evaluate(rules: &[Rule], tool_name: &str, input: &str) -> Option<Effect> {
    let mut last: Option<Effect> = None;
    let mut any_deny = false;
    for r in rules {
        if r.action.eq_ignore_ascii_case(tool_name) {
            let matched = r.content.as_ref().is_none_or(|c| c.matches(input));
            if matched {
                if r.effect == Effect::Deny {
                    any_deny = true;
                }
                last = Some(r.effect);
            }
        }
    }
    if any_deny {
        return Some(Effect::Deny);
    }
    last
}

/// Denied agent type names from Agent(Type) deny rules, so the agent tool
/// can tell a denial from an unknown type. Only exact type names; prefix
/// and glob agent denies are not type-specific.
pub fn denied_agent_types(rules: &[Rule]) -> std::collections::HashSet<String> {
    rules
        .iter()
        .filter(|r| r.action.eq_ignore_ascii_case("agent") && r.effect == Effect::Deny)
        .filter_map(|r| match r.content.as_ref()? {
            RuleContent::Exact(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eval_last_match_wins() {
        let rules = vec![
            Rule::new("bash", Effect::Ask).unwrap(),
            Rule::new("bash", Effect::Allow).unwrap(),
        ];
        assert_eq!(evaluate(&rules, "bash", "ls"), Some(Effect::Allow));
    }

    #[test]
    fn test_eval_deny_wins_over() {
        let rules = vec![
            Rule::new("bash", Effect::Deny).unwrap(),
            Rule::new("bash", Effect::Allow).unwrap(),
        ];
        assert_eq!(evaluate(&rules, "bash", "ls"), Some(Effect::Deny));
    }

    #[test]
    fn test_eval_no_match_returns() {
        let rules = vec![Rule::new("bash", Effect::Allow).unwrap()];
        assert_eq!(evaluate(&rules, "edit", "/tmp/a"), None);
    }

    #[test]
    fn test_eval_case_insensitive_tool() {
        let r = Rule::new("Bash", Effect::Allow).unwrap();
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "BASH", "ls"),
            Some(Effect::Allow)
        );
    }

    #[test]
    fn test_rule_rejects_empty_tool() {
        assert!(Rule::new("  ", Effect::Allow).is_err());
    }

    #[test]
    fn test_content_exact_matches() {
        let r = Rule::with_content(
            "bash",
            RuleContent::Exact("npm install".into()),
            Effect::Allow,
        )
        .unwrap();
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", "npm install"),
            Some(Effect::Allow)
        );
        // A different command does not match the exact content rule.
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", "npm uninstall"),
            None
        );
    }

    #[test]
    fn test_denied_agents_exact_deny() {
        let rules = vec![
            Rule::with_content("agent", RuleContent::Exact("explore".into()), Effect::Deny)
                .unwrap(),
            Rule::with_content("agent", RuleContent::Exact("plan".into()), Effect::Deny).unwrap(),
            // An allow on a type does not deny it.
            Rule::with_content("agent", RuleContent::Exact("verify".into()), Effect::Allow)
                .unwrap(),
            // A non-agent deny is irrelevant.
            Rule::new("bash", Effect::Deny).unwrap(),
            // A prefix agent deny is ignored — a type deny is always exact.
            Rule::with_content("agent", RuleContent::Prefix("ex".into()), Effect::Deny).unwrap(),
        ];
        let set = denied_agent_types(&rules);
        assert!(set.contains("explore"));
        assert!(set.contains("plan"));
        assert!(!set.contains("verify"));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_content_prefix_matches() {
        let r =
            Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", "npm install"),
            Some(Effect::Allow)
        );
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", "npm run build"),
            Some(Effect::Allow)
        );
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", "cargo build"),
            None
        );
    }

    #[test]
    fn test_content_glob_matches() {
        let r =
            Rule::with_content("bash", RuleContent::Glob("git *".into()), Effect::Allow).unwrap();
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", "git commit -m x"),
            Some(Effect::Allow)
        );
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", "git status"),
            Some(Effect::Allow)
        );
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", "cargo build"),
            None
        );
    }

    #[test]
    fn test_content_parse_colon_star() {
        match RuleContent::parse("npm:*") {
            RuleContent::Prefix(s) => assert_eq!(s, "npm"),
            other => panic!("expected prefix, got {other:?}"),
        }
    }

    #[test]
    fn test_content_parse_detects_glob() {
        assert!(matches!(RuleContent::parse("git *"), RuleContent::Glob(_)));
        assert!(matches!(RuleContent::parse("ls?"), RuleContent::Glob(_)));
    }

    #[test]
    fn test_plain_content_parses_exact() {
        assert!(matches!(
            RuleContent::parse("npm install"),
            RuleContent::Exact(_)
        ));
    }

    #[test]
    fn test_content_glob_invalid_never() {
        // An unbalanced bracket is an invalid glob; fail-closed (no match).
        let r = Rule::with_content("bash", RuleContent::Glob("[invalid".into()), Effect::Allow)
            .unwrap();
        assert_eq!(evaluate(std::slice::from_ref(&r), "bash", "[invalid"), None);
    }

    #[test]
    fn test_content_scoped_skips_empty() {
        // input None yields empty string, so a content-scoped rule never
        // matches a call with no input (tool-name-only rules still would).
        let r =
            Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
        assert_eq!(evaluate(std::slice::from_ref(&r), "bash", ""), None);
    }

    #[test]
    fn test_tool_only_matches_any() {
        let r = Rule::new("bash", Effect::Allow).unwrap();
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", ""),
            Some(Effect::Allow)
        );
        assert_eq!(
            evaluate(std::slice::from_ref(&r), "bash", "rm -rf /"),
            Some(Effect::Allow)
        );
    }

    #[test]
    fn test_input_content_extracts_command() {
        let v = serde_json::json!({"command": "ls -la"});
        assert_eq!(input_content("bash", Some(&v)), "ls -la");
    }

    #[test]
    fn test_input_content_extracts_path() {
        let v = serde_json::json!({"path": "/tmp/a.txt"});
        assert_eq!(input_content("edit", Some(&v)), "/tmp/a.txt");
    }

    #[test]
    fn test_input_content_extracts_url() {
        let v = serde_json::json!({"url": "https://example.com"});
        assert_eq!(input_content("webfetch", Some(&v)), "https://example.com");
    }

    #[test]
    fn test_input_content_none_empty() {
        assert_eq!(input_content("bash", None), "");
    }

    #[test]
    fn test_effect_parse_roundtrip() {
        for e in [Effect::Allow, Effect::Deny, Effect::Ask] {
            assert_eq!(Effect::parse(e.label()).unwrap(), e);
        }
        assert!(Effect::parse("bogus").is_err());
    }

    #[test]
    fn test_bash_prefix_single_command() {
        // A single command scopes to its first token.
        assert_eq!(bash_always_allow_prefix("ls -la /tmp"), Some("ls".into()));
    }

    #[test]
    fn test_bash_prefix_runner_compound() {
        // Runners scope to runner+subcommand (tighter than runner alone).
        assert_eq!(
            bash_always_allow_prefix("npm run build"),
            Some("npm run".into())
        );
        assert_eq!(
            bash_always_allow_prefix("cargo test --lib"),
            Some("cargo test".into())
        );
    }

    #[test]
    fn test_bash_prefix_destructive_allowed() {
        // Default-mode always-allow is the user's choice: rm/sudo/dd persist
        // a prefix rule (rm:*); the dangerous-rule stripper removes it in
        // classifier mode, and protected paths are guarded by the safety
        // layer regardless. The earlier "not eligible" refusal is dropped.
        assert_eq!(
            bash_always_allow_prefix("rm -rf /tmp/build"),
            Some("rm".into())
        );
        assert_eq!(
            bash_always_allow_prefix("sudo apt install foo"),
            Some("sudo".into())
        );
        assert_eq!(
            bash_always_allow_prefix("dd if=/dev/zero of=/dev/sda"),
            Some("dd".into())
        );
    }

    #[test]
    fn test_bash_prefix_compound_refused() {
        // A compound (A && B) cannot be scoped by one prefix rule.
        assert_eq!(bash_always_allow_prefix("ls && rm -rf /tmp"), None);
        assert_eq!(bash_always_allow_prefix("echo hi || echo bye"), None);
    }

    #[test]
    fn test_bash_prefix_runner_subcommand() {
        // A subcommand named like a destructive standalone command (cargo rm,
        // git rm) is the runner's version-controlled op, not the standalone
        // destructive command — scope to runner+subcommand, do not refuse.
        assert_eq!(
            bash_always_allow_prefix("cargo rm unused"),
            Some("cargo rm".into())
        );
        assert_eq!(
            bash_always_allow_prefix("git rm -f file"),
            Some("git rm".into())
        );
    }
}
