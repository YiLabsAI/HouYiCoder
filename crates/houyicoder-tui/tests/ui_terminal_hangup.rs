//! A session whose terminal closes must end, not spin.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::path::PathBuf;
use std::process::id;
use std::thread::sleep;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// The binary the terminal-level suites drive.
fn houyi_binary() -> PathBuf {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("target").join(profile).join("houyi"))
        .unwrap_or_else(|| PathBuf::from(format!("target/{profile}/houyi")))
}

/// Closing the terminal's other end ends the session.
///
/// A terminal that goes away leaves its descriptor readable with nothing to
/// read. The event reader then reports end-of-file instead of waiting, and a
/// loop that waits inside it never regains control: the session keeps running
/// with its terminal already gone, at a full core. The wait therefore runs on a
/// thread of its own, so the loop always gets control back to answer its own
/// exit decision.
#[test]
#[ignore]
fn test_hangup_ends_session() {
    let bin = houyi_binary();
    assert!(
        bin.exists(),
        "the terminal suites drive the built binary; build it first: {bin:?}"
    );
    let home = temp_dir().join(format!("houyi-hangup-{}", id()));
    create_dir_all(&home).expect("temp home");

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");
    let mut cmd = CommandBuilder::new(&bin);
    cmd.env("HOME", &home);
    cmd.env("TERM", "xterm-256color");
    cmd.env(
        houyicoder_config::ENV_HOUYICODER_CONFIG_HOME,
        home.join(".houyicoder"),
    );
    // Empty every supported key so the fixture cannot reach a real provider.
    cmd.env("DASHSCOPE_API_KEY", "");
    cmd.env("OPENAI_API_KEY", "");
    cmd.env(houyicoder_config::ENV_HOUYICODER_API_KEY, "");
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .unwrap_or_else(|e| panic!("spawn {bin:?}: {e}"));
    drop(pair.slave);
    // Let the session reach its first idle frame before the terminal goes away:
    // a session that never started is not the case this covers.
    sleep(Duration::from_secs(2));
    assert!(
        child.try_wait().expect("poll the child").is_none(),
        "the session ended before its terminal was closed"
    );
    drop(pair.master);

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ended = false;
    while Instant::now() < deadline {
        if child.try_wait().expect("poll the child").is_some() {
            ended = true;
            break;
        }
        sleep(Duration::from_millis(50));
    }
    if !ended {
        // A session that outlived its terminal would spin here for as long as
        // the host lives, so the failure path takes it down itself.
        drop(child.kill());
    }
    assert!(
        ended,
        "the session outlived its terminal: closing the other end must end it"
    );
}
