//! Session teardown over a real server: a swap drops the old session, which
//! drops the driver's command channel, which drops the client, which drops the
//! wire channel, so the old serve loop sees a clean disconnect and exits.

use std::thread::sleep;
use std::time::Duration;

use houyicoder_tui::agent_message::{ConnectionEvent, SessionMessage};
use houyicoder_tui::composition::build_app;

use crate::common::server_backed_handoff;

/// No orphan server task lingers after a swap. This is the foundation that
/// makes abandoning queued input safe: the abandoned items die with the old
/// server. Polled rather than awaited because the shared runtime runs the
/// serve loop on its own threads.
#[test]
fn test_tears_down_old_server() {
    let (bundle, startup, old_serve) = server_backed_handoff();
    let mut app = build_app(bundle, startup);
    let msg = app
        .poll_startup(Duration::from_secs(5))
        .expect("the handshake result arrives");
    assert!(
        matches!(msg, SessionMessage::Connection(ConnectionEvent::Ready)),
        "the driver announces readiness, got {msg:?}"
    );
    app.handle_agent_message(msg);
    assert!(
        !old_serve.is_finished(),
        "the old server is alive before the swap (handshake done, waiting on input)"
    );
    app.switch_session(server_backed_handoff().0);
    let mut tries = 0;
    while !old_serve.is_finished() && tries < 200 {
        sleep(Duration::from_millis(10));
        tries += 1;
    }
    assert!(
        old_serve.is_finished(),
        "the old serve loop must exit after the swap drops the session \
         (command channel drop -> driver exit -> client drop -> server \
         disconnect). Polled {tries} times; still running means an orphan \
         server leak."
    );
}
