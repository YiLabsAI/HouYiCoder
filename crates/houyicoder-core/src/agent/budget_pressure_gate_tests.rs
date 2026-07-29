//! System-prompt pressure gate: assert the assembled system prompt carries
//! no budget-pressure patterns. The system prompt is built by
//! SystemPrompt::build(cwd) which walks AGENTS.md — to isolate houyi text
//! from user-authored content, cwd is /tmp (no AGENTS.md).
//!
//! The runtime-injected messages (reminder, summary) are verified by the
//! integration test in tests/budget_pressure_gate.rs which captures real
//! CompletionRequests.

use crate::agent::prompt::SystemPrompt;

const PRESSURE_PATTERNS: &[&str] = &[
    r"\d+ of \d+ turns",
    r"\d+%\s*(full|used|remaining)",
    r"remaining.{0,10}turns",
    r"approaches?\s+the\s+context",
    r"context\s+limit",
    r"\bbudget\b",
];

fn assert_no_pressure(text: &str, source: &str) {
    for pat in PRESSURE_PATTERNS {
        let re = regex::Regex::new(&format!("(?i){pat}")).unwrap();
        assert!(
            !re.is_match(text),
            "pressure pattern '{pat}' found in {source}:\n{text}"
        );
    }
}

#[test]
fn test_system_prompt_no_pressure() {
    let prompt = SystemPrompt::build(std::path::Path::new("/tmp"));
    assert_no_pressure(&prompt.text, "system prompt");
}
