//! Real-binary PTY smoke test for the /hooks pane. #[ignore] (each spawns
//! the houyi binary + a PTY -- too slow for the commit gate). Run via
//! make test ui (builds the bin first) or
//! cargo test --test ui_hooks -- --ignored after cargo build --bin houyi.
//!
//! Industrial-usability proof for the hooks surface: open the pane and
//! assert the read-only subtitle ("N hooks configured") + the settings
//! hint render. The pane is read-only inspection of the framework +
//! configured hook events; configuration lives in settings.json, so the
//! hint points there. The inline unit layer (configured_count, sort +
//! detail index, event description) proves the logic; this layer proves a
//! user can open the pane and see the subtitle + hint, and that Esc
//! dismisses it.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, run_slash_command, session_on_working};

/// /hooks opens the pane and renders the "N hooks configured" subtitle +
/// the "edit settings.json to configure" hint. Esc at the event-list level
/// dismisses the pane back to the working screen (Esc at the detail level
/// steps back to the list first).
#[test]
#[ignore]
fn test_hooks_pane_subtitle_hint() {
    let mut s = session_on_working();
    run_slash_command(&mut s, "hooks");
    assert!(
        s.wait_for("hooks configured", RENDER_TIMEOUT),
        "hooks pane subtitle should render:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("edit settings.json"),
        "the read-only hint should point at settings.json:\n{}",
        s.output()
    );
    // Esc dismisses the pane back to the working screen.
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "Esc should close the hooks pane back to working:\n{}",
        s.output()
    );
    drop(s);
}
