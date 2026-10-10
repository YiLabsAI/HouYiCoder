//! Tests for the current development-cycle demo and its design, implementation,
//! review, verification, rewind, and rework transitions. This does not model a
//! complete software development lifecycle.

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;

use crate::composition;
use crate::state::{Divergence, Pane, Screen, Stage, TranscriptLine};
use crate::test_harness::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

#[test]
fn test_auto_start_task_enters() {
    let mut app = working();
    app.ui.input.editor.set("fix the login bug".to_string());
    app.submit_input();
    assert_eq!(
        app.workflow.stage,
        Stage::Design,
        "task should auto-start design"
    );
    assert_eq!(app.ui.navigation.pane, Pane::Spec);
    assert!(
        matches!(
            app.sessions.current.transcript.last(),
            Some(TranscriptLine::System(s)) if s.contains("drafting design")
        ),
        "should log the design-draft transition"
    );
}

#[test]
fn test_rewind_unapproves_artifact() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // Approval advances from design to planning.
    assert!(app.workflow.spec_artifact.approved);
    app.run_command(SlashCommand::Rewind);
    assert_eq!(app.workflow.stage, Stage::Design);
    assert!(
        !app.workflow.spec_artifact.approved,
        "rewind should un-approve the spec artifact"
    );
    assert!(
        matches!(
            app.sessions.current.transcript.last(),
            Some(TranscriptLine::System(s)) if s.contains("un-approved")
        ),
        "should log the un-approve note"
    );
}

#[test]
fn test_rewind_targeted_to_named() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // Approval advances to planning.
    app.approve_in_pane(); // Approval advances to implementation.
    app.ui.input.editor.set("/rewind spec".to_string());
    app.submit_input();
    assert_eq!(
        app.workflow.stage,
        Stage::Design,
        "targeted rewind to design"
    );
    assert!(!app.workflow.spec_artifact.approved);
}

#[test]
fn test_rework_real_finding() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // Approval advances from design to implementation.
    // Auto-advance walks pending changes in order and enters verification.
    for _ in 0..3 {
        app.approve_in_pane();
    }
    assert_eq!(app.workflow.stage, Stage::Verify);
    // Focus the real security finding.
    while app
        .workflow
        .review
        .current()
        .is_none_or(|f| f.verdict != "real")
    {
        app.navigate_pane(true);
        if app.workflow.review.focus == 0 {
            break;
        }
    }
    app.rework_in_pane();
    assert_eq!(
        app.workflow.stage,
        Stage::Implementing,
        "rework from review should go back to implementing"
    );
    assert_eq!(app.ui.navigation.pane, Pane::Diff);
    assert_eq!(
        app.workflow
            .spec_clauses
            .iter()
            .find(|c| c.id == "clause-2")
            .map(|c| c.status),
        Some(Divergence::Partial),
        "real finding's clause should regress to partial"
    );
}

#[test]
fn test_verify_fail_rework() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // Approval advances from design to implementation.
    // Approve the changes and review findings before machine verification.
    for _ in 0..3 {
        app.approve_in_pane();
    }
    for _ in 0..3 {
        app.approve_in_pane();
        app.navigate_pane(true);
    }
    assert_eq!(app.workflow.stage, Stage::Verify);
    // Simulate a failed verification directly because the dispatcher has no
    // dedicated test hook. The rework transition is the behavior under test.
    app.workflow.verify_result.passed = false;
    app.workflow.verify_result.checks = crate::composition::failing_checks();
    assert!(!app.workflow.verify_result.passed);
    // Approval cannot complete while verification is failing.
    app.approve_in_pane();
    assert_eq!(
        app.workflow.stage,
        Stage::Verify,
        "cannot complete on failed checks"
    );
    // Rework returns to implementation.
    app.rework_in_pane();
    assert_eq!(
        app.workflow.stage,
        Stage::Implementing,
        "verify rework should go back to implementing"
    );
    let out = render(&app);
    println!("--- after verify rework ---\n{out}\n--- end ---");
}
