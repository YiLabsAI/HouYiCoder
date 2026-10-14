//! Tests for this module.

use super::*;

#[test]
fn test_reporter_supports_trait_objects() {
    let _reporter: Box<dyn ToolProgressReporter> = Box::new(NoToolProgress);
}
