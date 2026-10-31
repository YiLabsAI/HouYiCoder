//! Tests for this module.

use std::sync::Arc;

use crate::agent::RunError;
use crate::agent::Runner;
use crate::agent::ToolRegistry;
use crate::agent::tests::runner_with;
use crate::provider::test_support::FakeProvider;
use houyicoder_context::SessionId;

fn bare_runner() -> Runner {
    runner_with(Arc::new(FakeProvider::text("unused")), ToolRegistry::new())
}

#[test]
fn test_second_drive_refused() {
    let runner = bare_runner();
    let held = runner.begin_drive().expect("a free runner admits one call");
    assert!(
        runner.begin_drive().is_none(),
        "a second call cannot take the slot of a running one"
    );
    drop(held);
    assert!(
        runner.begin_drive().is_some(),
        "the slot is free once the running call returns"
    );
}

#[test]
fn test_drive_released_on_drop() {
    let runner = bare_runner();
    {
        let _held = runner.begin_drive().expect("a free runner admits one call");
    }
    let _again = runner.begin_drive().expect("the drop released the slot");
}

/// A re-drive is a driving call like any other: it claims the slot, so a call
/// already writing the transcript is not replaced by a recovery the user did
/// not ask for.
#[tokio::test]
async fn test_recover_refused_while_driving() {
    let runner = bare_runner();
    let _held = runner.begin_drive().expect("a free runner admits one call");
    let err = runner
        .recover_turn(SessionId::new())
        .await
        .expect_err("the recovery does not take a held slot");
    assert!(
        matches!(err, RunError::Busy),
        "the refusal names the busy runner: {err:?}"
    );
}
