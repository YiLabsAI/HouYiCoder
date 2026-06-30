//! The wire form of the permission mode and the durable rule set, returned to
//! the frontend over the wire so the TUI renders /mode and /rules without
//! importing the permission crate. The engine types live in the permission
//! crate (a mode enum, a rule struct with an effect and an optional content
//! pattern); the wire form mirrors the shape but owns no behavior, so the
//! service boundary projects the engine state to this wire type and the TUI
//! renders it directly. The effect is narrowed to the two durable verdicts the
//! rule surface manages: allow and reject. An interactive ask is a transient
//! verdict, not a stored rule, so it has no wire variant here.

use serde::{Deserialize, Serialize};

/// The agent permission mode, wire form. Two modes — Manual asks before any
/// tool that declares it needs approval (read-only tools still auto-allow);
/// Auto allows safe operations and only asks for destructive ones (the
/// default — Auto is safe today because it still asks for destructive ops;
/// the recoverable invariant will let it handle destructive ops later). The
/// service boundary translates the engine mode to this wire form on /mode;
/// the TUI renders the label without pulling in the permission crate. Marked
/// non_exhaustive so a future variant fails safe rather than a match error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionMode {
    /// Ask before any tool that declares it needs approval; read-only tools
    /// still auto-allow.
    Manual,
    /// Allow safe operations; only ask for destructive ones. The default.
    Auto,
}

/// The effect a stored rule assigns to a matched tool action, wire form.
/// Three verdicts mirror the engine Effect: allow, reject (deny), and ask
/// (always prompt). Ask rules are session-scoped (kept in memory, not
/// persisted to disk) but they ARE stored rules the gate consults, so the
/// wire carries the variant — /rules shows them and the projection round-trips
/// them losslessly. snake_case on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionEffect {
    Allow,
    Reject,
    Ask,
}

/// The default effect for a rule constructed without one: allow (the most
/// common durable verdict an interactive always-allow lands).
impl Default for PermissionEffect {
    fn default() -> Self {
        Self::Allow
    }
}

/// A content pattern matched against the tool input string, wire form. The
/// three tiers mirror the engine rule shape: exact (literal full-string
/// match), prefix (input starts with this string), and glob (a case-insensitive
/// glob pattern). None means the rule matches the tool regardless of input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionRuleContent {
    Exact { value: String },
    Prefix { value: String },
    Glob { value: String },
}

/// Where a rule lives: user (shared across projects, in the home dir),
/// project (checked into the repo), local (a runtime temp, not committed),
/// session (in-memory, cleared on restart — a "don't ask again this session"
/// consent), or builtin (shipped with the binary, seeded at construction, the
/// user can disable but not edit). Mirrors the engine rule's scope; the wire
/// form lets the /permissions view render every rule without importing the
/// permission crate. snake_case on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleDestination {
    User,
    Project,
    Local,
    Session,
    Builtin,
}

impl RuleDestination {
    /// The default destination for a rule added without an explicit pick:
    /// project (repo-shared, the most common interactive always-allow target).
    pub const DEFAULT: Self = Self::Project;
}

impl Default for RuleDestination {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A durable permission rule, wire form. Pairs a tool name (action) with an
/// optional content pattern, an effect, and the destination (persistence
/// scope) the rule lands in. The service boundary projects the engine rule to
/// this wire form on /rules; the TUI renders the list without importing the
/// permission crate. content is None for a tool-level rule that matches
/// regardless of input. destination defaults to project so a legacy add that
/// omits it round-trips losslessly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<PermissionRuleContent>,
    pub effect: PermissionEffect,
    #[serde(default)]
    pub destination: RuleDestination,
}

/// One entry in the session verdict log, wire form. The server emits each
/// permission decision as an acpx/context/permission_decision notification
/// whose params carry the tool, the verdict (allow / deny), the scope, and the
/// call_id. This struct is the typed deserialization target for those params
/// the TUI accumulates verdicts from the live acpx stream into a typed log
/// (not stringly-typed JSON poking), so the /permission verdict-log render is
/// type-checked. The verdict is the durable label the engine verdict renders
/// to (allow / deny), not the transient Ask state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDecisionEntry {
    /// The tool the decision was for (e.g. "bash").
    pub tool: String,
    /// The durable verdict label the engine verdict renders to ("allow" /
    /// "deny"). A string, not an enum, because the server emits the engine
    /// verdict's own label and the TUI renders it verbatim.
    pub verdict: String,
    /// The scope string the decision covered (a command prefix, a path, ...).
    pub scope: String,
    /// The tool-call id the decision attached to, linking back to the
    /// trajectory tool_call row.
    pub call_id: String,
}

/// Why an approval prompt is being shown, wire form. Mirrors the engine
/// AskReason so the approval card can render a one-sentence explanation
/// without importing the permission crate. The source drives the wording and
/// which options the card offers (a system-safety ask hides the
/// remember-this-choice option). Marked non_exhaustive so a future source
/// fails safe rather than a match error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AskSource {
    /// An ask rule the user configured, including rules shipped as builtin
    /// defaults.
    UserRule,
    /// A protected path the agent must never write to silently.
    SystemSafety,
    /// A deterministic heuristic that flagged the call as risky.
    Detection,
    /// The tool itself declared that it needs approval.
    ToolNative,
    /// An unknown source from a newer engine. A legacy frontend renders a
    /// generic prompt instead of failing to deserialize.
    #[serde(other)]
    Unknown,
}

/// Why a call was rejected without asking, wire form. Carries the source so
/// the frontend can tell a user-configured deny from a headless fallback. The
/// sandbox variant is transitional: the engine emits it only while the
/// containment contract is unwired, and removes it once the fence rejects at
/// execution time. Marked non_exhaustive so a future source fails safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DenySource {
    /// A deny rule the user configured.
    UserRule,
    /// Headless operation turned an approval prompt into a rejection because
    /// there is no one to answer it.
    Headless,
}

/// Why a call was escalated to the user, wire form. The validator name is a
/// stable identifier the frontend uses as a metrics bucket key; detail is the
/// one sentence the user reads; containment_note is an optional dim line the
/// fence contributes when it is expected to reject a call the user is about to
/// approve (purely informational — the gate never turns it into a rejection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskReason {
    pub source: AskSource,
    /// The validator that produced the verdict. A string on the wire because
    /// the engine-side identifier is a static str the protocol boundary does
    /// not share by reference.
    pub validator: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment_note: Option<String>,
}

/// Why a call was rejected without asking, wire form. Pairs a source with the
/// validator name and a one-sentence detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DenyReason {
    pub source: DenySource,
    pub validator: String,
    pub detail: String,
}

/// Why a call was allowed, wire form. Flattened: the engine-side containment
/// variant carries a proof token the frontend cannot verify and has no use
/// for, so the wire form drops the token and keeps only the label. The
/// conversion is one-way (engine to wire); a reverse construction is
/// deliberately not provided, because rebuilding the proof from wire would
/// demote it to decoration. Marked non_exhaustive so a future variant fails
/// safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AllowReason {
    UserRule,
    Consent,
    /// The containment layer proved the call is fenced. The proof token stays
    /// on the host side; the wire carries only the label.
    Containment,
    ModeDefault,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_round_trips() {
        for mode in [PermissionMode::Manual, PermissionMode::Auto] {
            let json = serde_json::to_string(&mode).expect("serialize");
            let back: PermissionMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn test_mode_snake_case_tags() {
        let json = serde_json::to_string(&PermissionMode::Manual).expect("serialize");
        assert_eq!(json, "\"manual\"");
        let json = serde_json::to_string(&PermissionMode::Auto).expect("serialize");
        assert_eq!(json, "\"auto\"");
    }

    #[test]
    fn test_effect_round_trips() {
        for e in [
            PermissionEffect::Allow,
            PermissionEffect::Reject,
            PermissionEffect::Ask,
        ] {
            let json = serde_json::to_string(&e).expect("serialize");
            let back: PermissionEffect = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, e);
        }
        assert_eq!(
            serde_json::to_string(&PermissionEffect::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionEffect::Reject).unwrap(),
            "\"reject\""
        );
        assert_eq!(
            serde_json::to_string(&PermissionEffect::Ask).unwrap(),
            "\"ask\""
        );
    }

    #[test]
    fn test_content_round_trips_tier() {
        let exact = PermissionRuleContent::Exact {
            value: "npm install".into(),
        };
        let prefix = PermissionRuleContent::Prefix {
            value: "npm".into(),
        };
        let glob = PermissionRuleContent::Glob {
            value: "git *".into(),
        };
        for c in [exact, prefix, glob] {
            let json = serde_json::to_string(&c).expect("serialize");
            let back: PermissionRuleContent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, c);
        }
    }

    #[test]
    fn test_content_tags_by_kind() {
        let json = serde_json::to_string(&PermissionRuleContent::Prefix {
            value: "npm".into(),
        })
        .expect("serialize");
        assert!(json.contains("\"kind\""), "tagged by kind: {json}");
        assert!(json.contains("\"prefix\""), "snake_case variant: {json}");
        assert!(json.contains("\"value\""), "field name: {json}");
    }

    #[test]
    fn test_rule_round_trips_content() {
        let rule = PermissionRule {
            action: "bash".into(),
            content: Some(PermissionRuleContent::Prefix {
                value: "npm".into(),
            }),
            effect: PermissionEffect::Allow,
            destination: RuleDestination::User,
        };
        let json = serde_json::to_string(&rule).expect("serialize");
        let back: PermissionRule = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, rule);
    }

    #[test]
    fn test_round_trips_without_content() {
        let rule = PermissionRule {
            action: "bash".into(),
            content: None,
            effect: PermissionEffect::Reject,
            destination: RuleDestination::default(),
        };
        let json = serde_json::to_string(&rule).expect("serialize");
        let back: PermissionRule = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, rule);
        // content is skipped when None.
        assert!(!json.contains("content"), "None content skipped: {json}");
    }

    #[test]
    fn test_rule_destination_defaults_project() {
        // A legacy add that omits destination round-trips to project (the
        // default), so an older client that never set the field still lands
        // in the repo-shared scope.
        let json = r#"{"action":"bash","effect":"allow"}"#;
        let back: PermissionRule = serde_json::from_str(json).expect("deserialize");
        assert_eq!(back.destination, RuleDestination::Project);
    }

    #[test]
    fn test_rule_camel_case_keys() {
        let rule = PermissionRule {
            action: "bash".into(),
            content: None,
            effect: PermissionEffect::Allow,
            destination: RuleDestination::default(),
        };
        let json = serde_json::to_string(&rule).expect("serialize");
        assert!(json.contains("\"action\""), "camelCase action: {json}");
        assert!(json.contains("\"effect\""), "camelCase effect: {json}");
    }

    #[test]
    fn test_decision_entry_round_trips() {
        let entry = PermissionDecisionEntry {
            tool: "bash".into(),
            verdict: "allow".into(),
            scope: "npm install".into(),
            call_id: "call_42".into(),
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(json.contains("\"callId\""), "camelCase callId: {json}");
        let back: PermissionDecisionEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, entry);
    }

    #[test]
    fn test_permission_decision_decodes() {
        // The server emits the verdict notification params as this JSON shape;
        // the typed struct must decode it (camelCase callId), not the prior
        // snake_case call_id.
        let params = serde_json::json!({
            "callId": "call_7",
            "tool": "bash",
            "verdict": "deny",
            "scope": "rm -rf",
        });
        let entry: PermissionDecisionEntry =
            serde_json::from_value(params).expect("decode server params");
        assert_eq!(entry.tool, "bash");
        assert_eq!(entry.verdict, "deny");
        assert_eq!(entry.call_id, "call_7");
        assert_eq!(entry.scope, "rm -rf");
    }

    #[test]
    fn test_ask_source_round_trips() {
        for s in [
            AskSource::UserRule,
            AskSource::SystemSafety,
            AskSource::Detection,
            AskSource::ToolNative,
        ] {
            let json = serde_json::to_string(&s).expect("serialize");
            let back: AskSource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, s);
        }
    }

    #[test]
    fn test_ask_source_falls_back() {
        // A source a newer engine might emit decodes to Unknown, not an error.
        let back: AskSource = serde_json::from_str("\"yet_another_source\"").expect("fallback");
        assert_eq!(back, AskSource::Unknown);
    }

    #[test]
    fn test_deny_source_round_trips() {
        for s in [DenySource::UserRule, DenySource::Headless] {
            let json = serde_json::to_string(&s).expect("serialize");
            let back: DenySource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, s);
        }
    }

    #[test]
    fn test_ask_reason_round_trips() {
        let r = AskReason {
            source: AskSource::SystemSafety,
            validator: "protected_path".into(),
            detail: "writing to a protected path".into(),
            containment_note: Some("the fence may still block this".into()),
        };
        let json = serde_json::to_string(&r).expect("serialize");
        assert!(json.contains("\"containmentNote\""), "camelCase: {json}");
        let back: AskReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, r);
    }

    #[test]
    fn test_ask_reason_omits_note() {
        // A legacy engine that omits containment_note deserializes fine.
        let json = r#"{"source":"detection","validator":"rm","detail":"x"}"#;
        let back: AskReason = serde_json::from_str(json).expect("deserialize");
        assert_eq!(back.source, AskSource::Detection);
        assert!(back.containment_note.is_none());
    }

    #[test]
    fn test_allow_reason_round_trips() {
        for r in [
            AllowReason::UserRule,
            AllowReason::Consent,
            AllowReason::Containment,
            AllowReason::ModeDefault,
        ] {
            let json = serde_json::to_string(&r).expect("serialize");
            let back: AllowReason = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, r);
        }
    }
}
