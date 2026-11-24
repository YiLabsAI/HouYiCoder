//! A write-side check for credentials. The memory index is injected into every
//! request, so a key that lands in a record is sent to the model on every turn
//! and copied into the on-disk index. The check names the rule that fired and
//! never the matched text, so the refusal cannot repeat the secret.

/// High-confidence prefixes, each distinctive enough that ordinary prose does
/// not contain them. A prefix match names the provider, not the value.
const PREFIXES: &[(&str, &str)] = &[
    ("aws-access-key", "AKIA"),
    ("aws-access-key", "ASIA"),
    ("gcp-api-key", "AIza"),
    ("anthropic-api-key", "sk-ant-api"),
    ("openai-api-key", "sk-proj-"),
    ("github-pat", "ghp_"),
    ("github-fine-grained-pat", "github_pat_"),
    ("gitlab-pat", "glpat-"),
    ("slack-bot-token", "xoxb-"),
    ("slack-user-token", "xoxp-"),
    ("npm-token", "npm_"),
    ("huggingface-token", "hf_"),
    ("stripe-live-key", "sk_live_"),
];

/// The rule that fired for this text, or None when it carries no credential
/// pattern. A private key block is matched by its opening and closing banner.
pub fn first_match(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return Some("private-key");
    }
    PREFIXES
        .iter()
        .find(|(_, prefix)| text.contains(prefix))
        .map(|(rule, _)| *rule)
}
