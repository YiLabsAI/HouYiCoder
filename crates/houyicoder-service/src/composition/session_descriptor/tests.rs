//! Tests for this module.

use super::build_initial_descriptor;
use houyicoder_context::PromptCoverage;

/// A fresh session is observed from its first event, so the descriptor says
/// its log is covered and the first prompt recorded later really is first.
#[test]
fn test_initial_descriptor_claims_coverage() {
    let descriptor = build_initial_descriptor("test-model", None);
    assert_eq!(descriptor.prompt_coverage, PromptCoverage::Complete);
    assert!(descriptor.first_prompt.is_none());
    assert!(descriptor.last_prompt.is_none());
}
