//! The working-surface submit path's deferral rules.

use houyicoder_protocol::envelope::RequestId;

use crate::composition;
use crate::input::paste::PasteStore;
use crate::state::Screen;
use crate::test_harness::{connected_app, render_text};

/// A reset submitted while a direct shell command runs is deferred rather
/// than run locally: the host refuses the reset mid-command, so clearing the
/// frontend anyway would leave it showing a transcript the host does not
/// have, and the refusal would land on a screen already wiped.
#[test]
fn test_clear_waits_for_shell() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.system_line("kept line".to_string());
    app.sessions.current.shell.begin(RequestId(1));
    app.ui.input.editor.set("/clear".to_string());
    app.submit_input();
    assert_eq!(
        app.ui.input.queue.len(),
        1,
        "the reset waits in the pending queue"
    );
    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("clear: will run when the shell command finishes"),
        "the deferral names the work it waits on: {screen}"
    );
    assert!(
        screen.contains("kept line"),
        "the transcript was not reset: {screen}"
    );
}

/// A refused submit puts the draft back as typed, placeholder included: the
/// composer shows the same compact token, and the paste side table still
/// expands it, so the retry sends the pasted text instead of the token.
#[test]
fn test_refused_submit_restores_draft() {
    let mut app = connected_app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.shell.begin(RequestId(1));
    let token = app.ui.input.pasted.ingest("first\nsecond\nthird\nfourth");
    let draft = format!("{token} please");
    assert_ne!(
        token, "first\nsecond\nthird\nfourth",
        "the paste is referenced"
    );
    app.ui.input.editor.set(draft.clone());
    app.submit_input();
    assert_eq!(
        app.ui.input.editor.value(),
        draft,
        "the refused draft returns in the form it was typed"
    );
    assert_eq!(
        PasteStore::expand(app.ui.input.editor.value(), &app.ui.input.pasted),
        "first\nsecond\nthird\nfourth please",
        "the side table still expands the restored draft"
    );
    let screen = render_text(&app, 80, 24);
    assert!(
        screen.contains("a command is running"),
        "the refusal names the work blocking it: {screen}"
    );
}
