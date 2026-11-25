//! Real-binary PTY smoke test for the /lsp pane. #[ignore] (each spawns the
//! houyi binary + a PTY + a real language server child -- too slow for the
//! commit gate). Run via make suite ui (builds the bin first) or
//! cargo test --test ui_all ui_lsp:: -- --ignored after cargo build --bin
//! houyi.
//!
//! Industrial-usability proof for the management pane: open it from the
//! palette, read the list and the detail, restart to launch the server, and
//! toggle with the transcript receipt landing in the same frame as the
//! refreshed rows. The server is a real child process speaking the protocol
//! over its standard streams, so the pane is driven by the same manager a
//! session runs -- and the launch counter proves the pane never starts it
//! before an explicit action.

#![allow(clippy::unwrap_in_result)]

use crate::common::{self, Key, PtySession, RENDER_TIMEOUT};
use common::{make_temp_repo, run_slash_command};
use std::fs::read_to_string;
use std::fs::{create_dir_all, write};
use std::path::Path;
use std::path::PathBuf;

/// A minimal language server over Content-Length framing: it answers the
/// handshake and the shutdown, and ignores every notification. Each launch
/// appends a line to the state file named as the first argument, so the
/// test can prove a restart really launched a process and that an enable
/// did not.
const SERVER_SCRIPT: &str = r#"
import json, sys

state_path = sys.argv[1]
with open(state_path, "a") as handle:
    handle.write("launch\n")

def read_message():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            if length is None:
                continue
            body = sys.stdin.buffer.read(length)
            return json.loads(body)
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1])

def send(payload):
    body = json.dumps(payload).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    message = read_message()
    if message is None:
        break
    method = message.get("method", "")
    if "id" not in message:
        if method == "exit":
            break
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"],
              "result": {"capabilities": {}}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
"#;

/// Launch the binary with an isolated home whose settings mount one real
/// child server, plus the stub provider the session talks to. The working
/// directory is a fresh throwaway repo.
fn launch_with_server() -> (PtySession, PathBuf) {
    let home = common::fresh_temp_dir("lsp-home");
    let config_dir = home.join(".houyicoder");
    create_dir_all(&config_dir).expect("create config dir");
    let script_path = home.join("probe_lsp.py");
    write(&script_path, SERVER_SCRIPT).expect("write the server script");
    let state_path = home.join("launches.txt");
    let settings = config_dir.join("settings.json");
    let settings_json = serde_json::json!({
        "lspServers": {
            "probe": {
                "command": "python3",
                "args": [script_path.to_string_lossy(), state_path.to_string_lossy()],
                "extensionToLanguage": {".rs": "rust"}
            }
        }
    });
    write(
        &settings,
        serde_json::to_string_pretty(&settings_json).unwrap(),
    )
    .expect("write settings");
    let cwd = make_temp_repo("lsp");
    let script = r#"
        [
            [{"type":"Text","text":"ok"}]
        ]
    "#;
    let mut s = PtySession::launch_with_args(
        Some(script.to_string()),
        None,
        Some(home.clone()),
        Some(cwd),
        &[],
    );
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "login screen should render:\n{}",
        s.output()
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen should render:\n{}",
        s.output()
    );
    (s, state_path)
}

/// The launches the stub has recorded so far, one line per process start.
fn launches(path: &Path) -> usize {
    read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// The pane's whole lifecycle: /lsp opens the list with the server idle and
/// nothing launched; Enter opens the detail with its command; Esc returns; r
/// launches it; t disables and re-enables, each confirmation landing as one
/// transcript line beside the refreshed rows, and the re-enable leaves the
/// server idle rather than launching it; Esc closes the pane back to the
/// working screen.
#[test]
#[ignore]
fn test_pty_lsp_pane_lifecycle() {
    let (mut s, launches_path) = launch_with_server();
    run_slash_command(&mut s, "lsp");
    assert!(
        s.wait_for_screen("LSP", RENDER_TIMEOUT),
        "the pane header should render:\n{}",
        s.screen().contents()
    );
    // A repaint writes only the cells whose content changed, so the count
    // and the state are read from the rebuilt screen, not the byte stream.
    // The session never launches a language server on its own.
    assert!(
        s.wait_for_screen("1 servers · 0 running", RENDER_TIMEOUT),
        "the configured server should be listed and idle:\n{}",
        s.screen().contents()
    );
    assert_eq!(
        launches(&launches_path),
        0,
        "opening the session and the pane starts no language server"
    );

    // Enter opens the selected row's detail: the command as configured.
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("command", RENDER_TIMEOUT),
        "the detail should show the command:\n{}",
        s.screen().contents()
    );

    // Esc returns to the list, not out of the pane.
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for_screen("Up/Down to select", RENDER_TIMEOUT),
        "Esc at the detail should return to the list:\n{}",
        s.screen().contents()
    );

    // r asks for a fresh launch; the row reports running only after the
    // child answered the handshake.
    s.send_key(&Key::Char('r'));
    assert!(
        s.wait_for_screen("1 servers · 1 running", RENDER_TIMEOUT),
        "the restart should launch the server:\n{}",
        s.screen().contents()
    );
    assert_eq!(
        launches(&launches_path),
        1,
        "the restart started exactly one process"
    );

    // t disables: the confirmation is one transcript line and the row flips
    // on the same reply.
    s.send_key(&Key::Char('t'));
    assert!(
        s.wait_for_screen("lsp: probe disabled", RENDER_TIMEOUT),
        "the disable receipt should reach the transcript:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("1 servers · 0 running", RENDER_TIMEOUT),
        "the row should flip to disabled:\n{}",
        s.screen().contents()
    );

    // t again re-enables. The server was not indexing or running, so the
    // enable leaves it idle: the pane reports no running server rather than
    // launching one, which is what keeps a session from spawning every
    // configured server at once.
    s.send_key(&Key::Char('t'));
    assert!(
        s.wait_for_screen("lsp: probe enabled", RENDER_TIMEOUT),
        "the enable receipt should reach the transcript:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("1 servers · 0 running", RENDER_TIMEOUT),
        "the enable should not launch the server:\n{}",
        s.screen().contents()
    );
    assert_eq!(
        launches(&launches_path),
        1,
        "the enable launches nothing; only the explicit restart did"
    );

    // Esc closes the pane back to the working screen.
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "Esc should close the lsp pane back to working:\n{}",
        s.output()
    );
    drop(s);
}
