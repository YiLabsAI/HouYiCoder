//! Specification stage transition and verdict tests.

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;

use crate::composition;
use crate::state::{Divergence, Pane, Screen, Stage, Verdict};
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

#[test]
fn test_design_approve_advances() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    let out = render(&app);
    println!("--- /spec (design) ---\n{out}\n--- end ---");
    assert!(out.contains("spec"), "spec pane title missing");
    assert!(out.contains("acceptance:"), "acceptance missing");
    assert_eq!(app.stage, Stage::Design);
    // one design approval -> implement (spec + plan merged into design)
    app.approve_in_pane();
    let out = render(&app);
    println!("--- after design approve ---\n{out}\n--- end ---");
    assert_eq!(app.stage, Stage::Implementing);
    assert_eq!(app.pane, Pane::Diff);
    assert!(out.contains("diff approval"), "diff pane not shown");
    assert!(out.contains("change 1/3"), "change counter missing");
    assert!(
        app.spec_artifact.approved,
        "spec artifact should be approved"
    );
    assert!(
        app.plan_artifact.approved,
        "plan artifact should be approved"
    );
}

#[test]
fn test_implement_approve_advances() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement
    // approve all 3 changes via the per-pane action. Auto-advance moves focus
    // to the next pending change after each approve, so a repeated approve
    // walks every change and trips the all-approved transition to verify.
    for _ in 0..3 {
        app.approve_in_pane();
    }
    let out = render(&app);
    println!("--- after all changes approved ---\n{out}\n--- end ---");
    assert_eq!(
        app.stage,
        Stage::Verify,
        "should auto-advance to verify (agent review phase)"
    );
    assert_eq!(app.pane, Pane::Review);
    // requirement statuses moved unimpl -> partial (changes approved)
    assert!(
        app.spec_clauses
            .iter()
            .all(|c| c.status == Divergence::Partial)
    );
    assert!(out.contains("review findings"), "review pane not shown");
}

#[test]
fn test_review_signoff_advances() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement
    for _ in 0..3 {
        app.approve_in_pane();
    }
    // approve all 3 findings (agent review phase)
    for _ in 0..3 {
        app.approve_in_pane();
        app.navigate_pane(true);
    }
    let out = render(&app);
    println!("--- after all findings approved ---\n{out}\n--- end ---");
    assert_eq!(
        app.stage,
        Stage::Verify,
        "stage stays verify across the two phases"
    );
    assert_eq!(app.pane, Pane::Verify, "should move to machine-check phase");
    assert!(out.contains("verify result"), "verify pane not shown");
}

#[test]
fn test_spec_chain_completes() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement
    for _ in 0..3 {
        app.approve_in_pane();
    }
    for _ in 0..3 {
        app.approve_in_pane();
        app.navigate_pane(true);
    }
    app.approve_in_pane(); // complete machine check -> done
    let out = render(&app);
    println!("--- after verify complete ---\n{out}\n--- end ---");
    assert_eq!(app.stage, Stage::Done);
    assert!(out.contains("DONE"), "completion indicator missing");
    assert!(
        app.spec_clauses
            .iter()
            .all(|c| c.status == Divergence::Satisfied),
        "clauses should be satisfied after verify"
    );
}

#[test]
fn test_reject_hunk_state() {
    let mut app = working();
    app.run_command(SlashCommand::Implement);
    app.reject_in_pane();
    let out = render(&app);
    println!("--- after hunk reject ---\n{out}\n--- end ---");
    assert_eq!(app.diff.current().unwrap().approved, Verdict::Rejected);
    assert!(out.contains("rejected"), "rejected state not visible");
}
