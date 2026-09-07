//! Real-binary PTY tests for the multi-agent interaction UX. Each test
//! spawns the binary under a PTY with a scripted stub provider so the full
//! chain runs end-to-end. The matrix covers each interaction path a user
//! can take. Slow, so ignored by default.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, pty_session_scripted, pty_session_slow_scripted};
use std::time::Duration;

/// A child text long enough that the collapsed fold summary truncates it.
/// Head and tail are distinct so a collapsed/expanded assertion can tell
/// summary from full content.
const LONG_CHILD: &str = "This is a long child analysis that exceeds the one-line fold summary limit so the collapsed head truncates it with an ellipsis while the expanded view shows the full text including this trailing sentinel.";

/// The grace window after which a completed pill row retires when the user
/// is not viewing it. Tests wait past this to prove the pin holds + the
/// post-exit retire fires. Mirrors the FLEET_GRACE constant in agent_message.
const FLEET_GRACE: Duration = Duration::from_secs(5);

#[path = "ui_multiagent/async_flow.rs"]
mod async_flow;
#[path = "ui_multiagent/esc.rs"]
mod esc;
#[path = "ui_multiagent/panes.rs"]
mod panes;
#[path = "ui_multiagent/pill.rs"]
mod pill;
#[path = "ui_multiagent/sync.rs"]
mod sync;
#[path = "ui_multiagent/teammate.rs"]
mod teammate;
