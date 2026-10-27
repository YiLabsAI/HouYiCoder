//! Recap PTY journey: a session the host summarized while the user was away
//! shows that summary when it is reopened. The unit tests cover the row's
//! shape; this covers the row reaching a real terminal.

#![allow(clippy::unwrap_in_result)]

use houyicoder_context::RecapSource;
use houyicoder_core::{EventId, SessionEvent};

use crate::common::{self, PtySession, RENDER_TIMEOUT, fresh_temp_dir};

const RECAP_TEXT: &str = "the parser fix is in; the failing test comes next";

/// A reopened session draws the summary the host wrote while the user was
/// away, as a notice at the transcript tail.
#[test]
#[ignore]
fn test_resume_shows_recap() {
    let sessions_dir = fresh_temp_dir("sessions-recap-notice");
    let sid = "55555555-5555-5555-5555-555555555555";
    common::seed_session_events(
        &sessions_dir,
        sid,
        "recap-model",
        &[
            SessionEvent::UserInput {
                text: "fix the parser".into(),
            },
            SessionEvent::AwayRecap {
                text: RECAP_TEXT.into(),
                last_user_event: EventId::new(),
                as_of_event: EventId::new(),
                source: RecapSource::Away,
                model: "fast-1".into(),
                usage: None,
            },
        ],
    );
    let mut s = PtySession::launch_with_sessions_dir(
        None,
        None,
        None,
        None,
        &["--resume".to_string(), sid.to_string()],
        sessions_dir,
    );
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen after resume:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_plain(&format!("recap: {RECAP_TEXT}"), RENDER_TIMEOUT),
        "the host's summary is on screen:\n{}",
        s.output_plain()
    );
}
