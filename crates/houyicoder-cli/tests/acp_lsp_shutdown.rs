//! An ACP session that starts a language server ends it when the client
//! closes: the server receives shutdown and its process exits, so no child
//! outlives the session that started it.

// The harness launches the product binary, which is not an engine spawn.
#![allow(clippy::disallowed_methods)]
// The stub language server runs on python3, which a Windows CI host does not
// guarantee on PATH. The contract this test pins is platform-neutral, so the
// probe lives on unix rather than carrying a second interpreter path.
#![cfg(unix)]

use houyicoder_config::ENV_HOUYICODER_CONFIG_HOME;
use houyicoder_config::ENV_HOUYICODER_MCP_SERVERS;
use serde_json::Value;
use serde_json::json;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::write;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// The scripted model: write one Rust file, ask the language server about it,
/// then end the turn. The lsp tool only registers when a routable server is
/// configured, so this call is also the proof the server was mounted.
const SCRIPT: &str = r#"[
  [{"type": "ToolCall", "id": "c1", "name": "write", "input": {"path": "probe.rs", "content": "fn probe() {}\n"}}],
  [{"type": "ToolCall", "id": "c2", "name": "lsp", "input": {"operation": "hover", "filePath": "probe.rs", "line": 1, "character": 4}}],
  [{"type": "Text", "text": "done"}]
]"#;

/// A minimal language server: it answers the handshake and a hover, records
/// each lifecycle word to the state file, and exits on the exit notification.
const SERVER_SCRIPT: &str = r#"
import json, sys

state_path = sys.argv[1]

def record(word):
    with open(state_path, "a") as handle:
        handle.write(word + "\n")

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
            return json.loads(sys.stdin.buffer.read(length))
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1])

def send(payload):
    body = json.dumps(payload).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

record("start")
while True:
    message = read_message()
    if message is None:
        # The client's connection ended, which is the process's cue to stop.
        record("exit")
        break
    method = message.get("method", "")
    if "id" not in message:
        if method == "exit":
            record("exit")
            break
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"capabilities": {}}})
    elif method == "shutdown":
        record("shutdown")
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "textDocument/hover":
        send({"jsonrpc": "2.0", "id": message["id"],
              "result": {"contents": {"kind": "plaintext", "value": "hover answer"}}})
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
"#;

fn scratch(name: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-acp-lsp-{name}-{}", std::process::id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("scratch dir");
    dir
}

fn spawn_agent(launch: &Path, home: &Path) -> (Child, Receiver<Value>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_houyi"))
        .arg("--acp")
        .current_dir(launch)
        .env("HOME", home)
        .env("USERPROFILE", home)
        // The gate runner injects a config home for every test process, and
        // the child would read its settings from there instead of the file
        // this test writes. The child reads the same tree the test writes.
        .env(ENV_HOUYICODER_CONFIG_HOME, home.join(".houyicoder"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove(ENV_HOUYICODER_MCP_SERVERS)
        .env("HOUYICODER_SESSIONS_DIR", home.join("sessions"))
        .env("HOUYICODER_TEST_STUB_RESPONSE_SCRIPT", SCRIPT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the agent");
    let stdout = child.stdout.take().expect("stdout pipe");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line)
                && tx.send(value).is_err()
            {
                return;
            }
        }
    });
    (child, rx)
}

fn send(child: &mut Child, frame: Value) {
    let stdin = child.stdin.as_mut().expect("stdin pipe");
    writeln!(stdin, "{frame}").expect("write frame");
    stdin.flush().expect("flush frame");
}

/// Wait for the reply carrying the given id, allowing any permission card the
/// turn raises on the way.
fn reply_to(child: &mut Child, rx: &Receiver<Value>, id: i64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let value = rx.recv_timeout(left).expect("reply before the deadline");
        if value.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            let request = value["id"].clone();
            send(
                child,
                json!({
                    "jsonrpc": "2.0", "id": request,
                    "result": {"outcome": {"outcome": "selected", "optionId": "allow"}},
                }),
            );
            continue;
        }
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            return value;
        }
    }
}

/// A language server started by the session receives shutdown and exits when
/// the client closes, so the child does not outlive the session.
#[test]
fn test_acp_close_stops_server() {
    let base = scratch("shutdown");
    let launch = base.join("launch");
    let project = base.join("project");
    let home = base.join("home");
    create_dir_all(&launch).expect("launch dir");
    create_dir_all(&project).expect("project dir");
    create_dir_all(home.join(".houyicoder")).expect("config dir");
    let server_path = base.join("probe_lsp.py");
    write(&server_path, SERVER_SCRIPT).expect("server script");
    let state_path = base.join("state.txt");
    write(
        home.join(".houyicoder/settings.json"),
        serde_json::to_string_pretty(&json!({
            "lspServers": {
                "probe": {
                    "command": "python3",
                    "args": [server_path.to_string_lossy(), state_path.to_string_lossy()],
                    "extensionToLanguage": {".rs": "rust"},
                },
            },
        }))
        .expect("settings encode"),
    )
    .expect("settings file");

    let (mut child, rx) = spawn_agent(&launch, &home);
    send(
        &mut child,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}},
        }),
    );
    reply_to(&mut child, &rx, 1);
    send(
        &mut child,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": project.to_string_lossy(), "mcpServers": []},
        }),
    );
    let opened = reply_to(&mut child, &rx, 2);
    let session = opened["result"]["sessionId"]
        .as_str()
        .expect("session id in the reply")
        .to_string();
    send(
        &mut child,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {"sessionId": session, "prompt": [{"type": "text", "text": "probe"}]},
        }),
    );
    reply_to(&mut child, &rx, 3);

    // Closing the client ends the serve loop, which ends the session's
    // language servers before the process exits.
    drop(child.stdin.take());
    let status = child.wait().expect("wait for the agent to exit");
    assert!(
        status.success(),
        "the agent process exits cleanly after the session ends: {status:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = read_to_string(&state_path).unwrap_or_default();
        if state.lines().any(|line| line == "shutdown") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the language server receives shutdown when the session ends; state: {state}"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let state = read_to_string(&state_path).expect("state file");
    let starts = state.lines().filter(|line| *line == "start").count();
    assert_eq!(
        starts, 1,
        "the session started the server exactly once, with no relaunch: {state}"
    );
    assert!(
        state.lines().any(|line| line == "shutdown"),
        "the server received the shutdown request: {state}"
    );
    assert!(
        state.lines().any(|line| line == "exit"),
        "the server process exited after the shutdown: {state}"
    );
    drop(remove_dir_all(&base));
}
