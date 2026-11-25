//! Real-binary PTY smoke test for the /mcp pane. #[ignore] (each spawns
//! the houyi binary + a PTY + a real child server -- too slow for the
//! commit gate). Run via make suite ui (builds the bin first) or
//! cargo test --test ui_all ui_mcp:: -- --ignored after cargo build --bin
//! houyi.
//!
//! Industrial-usability proof for the management pane: open it from the
//! palette, read the list and the detail, reconnect, toggle with the
//! transcript receipt landing in the same frame as the refreshed rows, and
//! leave the pane back at the working screen. The server is a real child
//! process speaking the protocol over its standard streams, so the pane is
//! driven by the same manager a real session runs.

#![allow(clippy::unwrap_in_result)]

use crate::common::{self, Key, PtySession, RENDER_TIMEOUT};
use common::{make_temp_repo, run_slash_command};
use std::fs::{create_dir_all, write};

/// A minimal external tool server: it answers the handshake, lists one tool
/// per handshake, and lists no resources, over newline-delimited JSON on its
/// standard streams. A reconnect starts a fresh process, so the handshake
/// count is kept in the state file named as the second argument; the tool
/// count it drives is what lets the test prove a reconnect really ran
/// rather than that the pane kept rendering.
const SERVER_SCRIPT: &str = r#"
import json, sys
state_path = sys.argv[1]
try:
    with open(state_path) as handle:
        generation = int(handle.read().strip() or "0")
except (OSError, ValueError):
    generation = 0
generation += 1
with open(state_path, "w") as handle:
    handle.write(str(generation))
for line in sys.stdin:
    try:
        req = json.loads(line)
    except ValueError:
        continue
    if "id" not in req:
        continue
    method = req.get("method", "")
    if method == "initialize":
        result = {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "pty-probe", "version": "1.0.%d" % generation},
            "instructions": "probe instructions",
        }
    elif method == "tools/list":
        tools = [{
            "name": "probe_tool",
            "description": "a probe tool",
            "inputSchema": {"type": "object"},
        }]
        for extra in range(1, generation + 1):
            tools.append({
                "name": "probe_gen_%d" % extra,
                "description": "handshake %d" % extra,
                "inputSchema": {"type": "object"},
            })
        result = {"tools": tools}
    elif method == "resources/list":
        result = {"resources": []}
    else:
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}), flush=True)
"#;

/// Launch the binary with an isolated home whose settings mount one real
/// child server, plus the stub provider the session talks to. The working
/// directory is a fresh throwaway repo.
fn launch_with_server() -> PtySession {
    let home = common::fresh_temp_dir("mcp-home");
    let config_dir = home.join(".houyicoder");
    create_dir_all(&config_dir).expect("create config dir");
    let script_path = home.join("probe_server.py");
    write(&script_path, SERVER_SCRIPT).expect("write the server script");
    let state_path = home.join("probe_generation.txt");
    let settings = config_dir.join("settings.json");
    let settings_json = serde_json::json!({
        "mcpServers": {
            "probe": {
                "command": "python3",
                "args": [script_path.to_string_lossy(), state_path.to_string_lossy()]
            }
        }
    });
    write(
        &settings,
        serde_json::to_string_pretty(&settings_json).unwrap(),
    )
    .expect("write settings");
    let cwd = make_temp_repo("mcp");
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
    s
}

/// The pane's whole lifecycle: /mcp opens the list and the server connects;
/// Enter opens the detail with its transport and tool; Esc returns; r asks
/// for a fresh handshake; t disables and re-enables, each confirmation
/// landing as one transcript line beside the refreshed rows; Esc closes the
/// pane back to the working screen.
#[test]
#[ignore]
fn test_pty_mcp_pane_lifecycle() {
    let mut s = launch_with_server();
    run_slash_command(&mut s, "mcp");
    assert!(
        s.wait_for("MCP", RENDER_TIMEOUT),
        "the pane header should render:\n{}",
        s.output()
    );
    // A repaint writes only the cells whose content changed, so the count
    // and the state are read from the rebuilt screen, not the byte stream.
    assert!(
        s.wait_for_screen("1 servers · 1 connected", RENDER_TIMEOUT),
        "the configured server should be listed and connected:\n{}",
        s.screen().contents()
    );

    // Enter opens the selected row's detail: transport and the tool it
    // provides, above the diagnostics.
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("transport", RENDER_TIMEOUT),
        "the detail should show the transport:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_screen("probe_tool", RENDER_TIMEOUT),
        "the detail should list the server's tool:\n{}",
        s.screen().contents()
    );

    // Esc returns to the list, not out of the pane.
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for_screen("Up/Down to select", RENDER_TIMEOUT),
        "Esc at the detail should return to the list:\n{}",
        s.screen().contents()
    );

    // r asks for a fresh handshake. Each handshake makes the stub list one
    // more tool, so the tool count on the row is the proof the reconnect
    // really ran: it moves only when a new connection answered.
    assert!(
        s.wait_for_screen("2 tools", RENDER_TIMEOUT),
        "the first handshake's tool list shows:\n{}",
        s.screen().contents()
    );
    s.send_key(&Key::Char('r'));
    assert!(
        s.wait_for_screen("3 tools", RENDER_TIMEOUT),
        "the reconnect's fresh handshake shows through the new tool:\n{}",
        s.screen().contents()
    );

    // t disables: the confirmation is one transcript line and the row
    // flips on the same reply. The zero count is the state only the
    // disabled row produces.
    s.send_key(&Key::Char('t'));
    assert!(
        s.wait_for_screen("mcp: probe disabled", RENDER_TIMEOUT),
        "the disable receipt should reach the transcript:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("1 servers · 0 connected", RENDER_TIMEOUT),
        "the row should flip to disabled:\n{}",
        s.screen().contents()
    );

    // t again re-enables, proving the parked supervisor comes back: the
    // connected count returns only through a real second handshake.
    s.send_key(&Key::Char('t'));
    assert!(
        s.wait_for_screen("mcp: probe enabled", RENDER_TIMEOUT),
        "the enable receipt should reach the transcript:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_screen("1 servers · 1 connected", RENDER_TIMEOUT),
        "the server should reconnect after the enable:\n{}",
        s.screen().contents()
    );

    // Esc closes the pane back to the working screen.
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "Esc should close the mcp pane back to working:\n{}",
        s.output()
    );
    drop(s);
}
