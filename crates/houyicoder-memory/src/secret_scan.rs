//! A write-side check for credentials. The memory index is injected into every
//! request, so a key that lands in a record is sent to the model on every turn
//! and copied into the on-disk index. The check names the rule that fired and
//! never the matched text, so the refusal cannot repeat the secret.

/// Access key ids carry a fixed alphabet and length, so a word that merely
/// starts with the prefix (a region name, an acronym) does not match.
const ACCESS_KEY_IDS: &[&str] = &["AKIA", "ASIA"];

/// Prefix rules: the rule name, the literal prefix, and the shortest suffix
/// that makes the prefix a credential rather than an ordinary identifier.
/// A short suffix is what tells npm_registry and hf_hub apart from a key.
const TOKEN_RULES: &[(&str, &str, usize)] = &[
    ("anthropic-api-key", "sk-ant-api", 16),
    ("openai-api-key", "sk-proj-", 16),
    ("github-pat", "ghp_", 20),
    ("github-fine-grained-pat", "github_pat_", 20),
    ("gitlab-pat", "glpat-", 16),
    ("slack-bot-token", "xoxb-", 12),
    ("slack-user-token", "xoxp-", 12),
    ("npm-token", "npm_", 20),
    ("huggingface-token", "hf_", 20),
    ("stripe-live-key", "sk_live_", 16),
    ("gcp-api-key", "AIza", 30),
];

/// Whether a suffix character belongs to a token: letters, digits, and the two
/// separators providers use inside a key.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Whether the character before an index starts a word, so a prefix embedded
/// inside a longer identifier is not read as a credential.
fn starts_word(text: &str, at: usize) -> bool {
    text[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '-')
}

/// The rule that fired for this text, or None when it carries no credential
/// pattern. A private key block is matched by its opening and closing banner.
pub fn first_match(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return Some("private-key");
    }
    for id in ACCESS_KEY_IDS {
        for (at, _) in text.match_indices(id) {
            if !starts_word(text, at) {
                continue;
            }
            let suffix = &text[at + id.len()..];
            let body: String = suffix
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            let after = suffix.chars().nth(body.len());
            if body.len() == 16
                && body
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c))
                && after.is_none_or(|c| !c.is_alphanumeric())
            {
                return Some("aws-access-key");
            }
        }
    }
    for (id, prefix, min_suffix) in TOKEN_RULES {
        for (at, _) in text.match_indices(prefix) {
            if !starts_word(text, at) {
                continue;
            }
            let suffix = &text[at + prefix.len()..];
            let run = suffix.chars().take_while(|c| is_token_char(*c)).count();
            if run >= *min_suffix {
                return Some(id);
            }
        }
    }
    None
}
