//! The workspace a client names in session/new is where the session's tools
//! run. A desktop host launches the agent from its own directory and sends
//! the project path in the request, so the launch directory must not decide
//! where a relative write lands.

use serde_json::Value;
use serde_json::json;
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::read_to_string;
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

/// A scripted model: the first reply asks to write hello.txt, the second
/// ends the turn.
const SCRIPT: &str = r#"[
  [{"type": "ToolCall", "id": "call_1", "name": "write", "input": {"path": "hello.txt", "content": "from the agent"}}],
  [{"type": "Text", "text": "done"}]
]"#;

fn scratch(name: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-acp-{name}-{}", std::process::id()));
    create_dir_all(&dir).expect("scratch dir");
    dir
}

// The harness launches the product binary, which is not an engine spawn.
#[allow(clippy::disallowed_methods)]
fn spawn_agent(launch: &Path, home: &Path) -> (Child, Receiver<Value>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_houyi"))
        .arg("--acp")
        .current_dir(launch)
        .env("HOME", home)
        .env("USERPROFILE", home)
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

/// Wait for the reply carrying the given id, answering any permission card
/// the turn raises on the way.
fn reply_to(child: &mut Child, rx: &Receiver<Value>, id: i64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let value = rx.recv_timeout(left).expect("reply before the deadline");
        if value.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            let options = value["params"]["options"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let choice = options
                .iter()
                .find_map(|o| o.get("optionId").and_then(Value::as_str))
                .unwrap_or("allow_once");
            let ask_id = value["id"].clone();
            send(
                child,
                json!({
                    "jsonrpc": "2.0",
                    "id": ask_id,
                    "result": {"outcome": {"outcome": "selected", "optionId": choice}},
                }),
            );
            continue;
        }
        if value.get("id").and_then(Value::as_i64) == Some(id) {
            return value;
        }
    }
}

fn initialize(child: &mut Child, rx: &Receiver<Value>) {
    send(
        child,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}},
        }),
    );
    reply_to(child, rx, 1);
}

/// A relative write lands in the directory session/new names, not in the
/// directory the process was launched from.
#[test]
fn test_session_cwd_roots_tools() {
    let base = scratch("workspace");
    let launch = base.join("launch");
    let project = base.join("project");
    create_dir_all(&launch).expect("launch dir");
    create_dir_all(&project).expect("project dir");

    let (mut child, rx) = spawn_agent(&launch, &base);
    initialize(&mut child, &rx);

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
            "params": {"sessionId": session, "prompt": [{"type": "text", "text": "write it"}]},
        }),
    );
    let replied = reply_to(&mut child, &rx, 3);
    assert_eq!(
        replied["result"]["stopReason"].as_str(),
        Some("end_turn"),
        "{replied}"
    );

    let written = project.join("hello.txt");
    assert!(
        written.exists(),
        "the write landed in the session workspace: {}",
        written.display()
    );
    assert_eq!(
        read_to_string(&written).expect("read the written file"),
        "from the agent"
    );
    assert!(
        !launch.join("hello.txt").exists(),
        "the launch directory stays untouched"
    );

    drop(child.stdin.take());
    drop(child.wait());
}

/// A request that names the launch directory itself still opens a session,
/// so a client that sends no project keeps working.
#[test]
fn test_launch_dir_defaults() {
    let base = scratch("default");
    let launch = base.join("launch");
    create_dir_all(&launch).expect("launch dir");

    let (mut child, rx) = spawn_agent(&launch, &base);
    initialize(&mut child, &rx);
    send(
        &mut child,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": launch.to_string_lossy(), "mcpServers": []},
        }),
    );
    let opened = reply_to(&mut child, &rx, 2);
    assert!(opened["result"]["sessionId"].is_string(), "{opened}");

    drop(child.stdin.take());
    drop(child.wait());
}
