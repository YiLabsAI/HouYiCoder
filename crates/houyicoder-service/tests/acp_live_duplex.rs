//! Real subprocess pipe tests for the ACP server: the stdio entry runs as a
//! child process and the test drives raw JSON-RPC over its stdin and stdout.
//! These cover streaming before EOF, cancel against a live run, mode and
//! model switching, and typed rejection of non-text content blocks.
//!
//! The example is the fixture. It builds on first use and is rebuilt every
//! run so an earlier revision's binary cannot answer for the current wire.

#![allow(clippy::disallowed_methods)] // test scaffolding spawns the built example, not an engine call

use std::env::current_dir;
use std::env::var_os;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;
use std::process::{Command, Stdio};
use std::sync::Once;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;

/// Resolve the example binary the way the enclosing cargo run placed it,
/// honouring CARGO_TARGET_DIR so a shared or per-worktree target works.
fn binary_path() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let base = match var_os("CARGO_TARGET_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                dir
            } else {
                current_dir().expect("current dir").join(dir)
            }
        }
        None => root.join("target"),
    };
    let name = if cfg!(windows) {
        "acp_stdio.exe"
    } else {
        "acp_stdio"
    };
    base.join("debug").join("examples").join(name)
}

/// Build the example once per test run. Rebuilding rather than reusing a
/// present binary matters: a binary left from an earlier revision would
/// exercise the old wire format while these assertions describe the new one.
fn ensure_built() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let status = Command::new("cargo")
            .args(["build", "--example", "acp_stdio"])
            .status()
            .expect("cargo build --example acp_stdio");
        assert!(status.success(), "build acp_stdio example");
    });
}

/// Spawn the example with the given environment and piped stdio.
fn spawn_server(env: &[(&str, &str)]) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    ensure_built();
    let mut command = Command::new(binary_path());
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn acp_stdio");
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    (child, stdin, BufReader::new(stdout))
}

/// Send one NDJSON line and return the next stdout line that parses as JSON.
fn send_and_recv(stdin: &mut impl Write, stdout: &mut impl BufRead, frame: &str) -> String {
    stdin.write_all(frame.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    recv(stdout)
}

/// Read one stdout line, asserting it is a complete JSON frame.
fn recv(stdout: &mut impl BufRead) -> String {
    let mut line = String::new();
    let n = stdout.read_line(&mut line).expect("read reply");
    assert!(n > 0, "server closed stdout");
    let line = line.trim();
    serde_json::from_str::<Value>(line)
        .unwrap_or_else(|e| panic!("stdout line is not a JSON frame ({e}): {line:?}"));
    line.to_string()
}

/// Run the initialize + session/new handshake and return the bound id.
fn handshake(stdin: &mut impl Write, stdout: &mut impl BufRead) -> (String, Value) {
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    drop(send_and_recv(stdin, stdout, &init.to_string()));
    let cwd = current_dir().expect("current dir");
    let new_session = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy(), "mcpServers": []},
    });
    let reply = send_and_recv(stdin, stdout, &new_session.to_string());
    let value: Value = serde_json::from_str(&reply).expect("session/new reply parses");
    let sid = value["result"]["sessionId"]
        .as_str()
        .expect("session id reported")
        .to_string();
    (sid, value["result"].clone())
}

fn prompt_frame(sid: &str, id: i64) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {"sessionId": sid, "prompt": [{"type": "text", "text": "hi"}]},
    })
    .to_string()
}

/// A prompt streams session/update chunks while stdin stays open, and the
/// final reply ends the turn.
#[test]
fn test_acp_streams_before_eof() {
    let (mut child, mut stdin, mut stdout) =
        spawn_server(&[("HOUYICODER_TEST_STUB_DELAY_MS", "30")]);
    let (sid, _) = handshake(&mut stdin, &mut stdout);
    drop(send_and_recv(
        &mut stdin,
        &mut stdout,
        &prompt_frame(&sid, 3),
    ));

    let mut saw_update = false;
    let mut reply = None;
    for _ in 0..256 {
        let line = recv(&mut stdout);
        if line.contains("session/update") {
            saw_update = true;
        }
        if line.contains(r#""id":3"#) {
            reply = Some(line);
            break;
        }
    }
    // stdin is still open here: the updates arrived before EOF.
    assert!(saw_update, "session/update streamed before EOF");
    let reply = reply.expect("prompt reply");
    assert!(reply.contains(r#""stopReason":"end_turn""#), "{reply}");

    drop(stdin);
    drop(child.wait());
}

/// A cancel sent against a live run stops the turn well within the budget
/// and replies with the cancelled stop reason.
#[test]
fn test_acp_cancel_aborts_runner() {
    let (mut child, mut stdin, mut stdout) = spawn_server(&[("HOUYICODER_ACP_STUB_HANG", "1")]);
    let (sid, _) = handshake(&mut stdin, &mut stdout);
    let prompt = prompt_frame(&sid, 3);
    let cancel =
        json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}});
    // The prompt and cancel go out together so the run is live when the
    // cancel is read, matching a client that cancels mid-generation.
    let batch = format!("{prompt}\n{cancel}\n");
    let start = Instant::now();
    stdin.write_all(batch.as_bytes()).unwrap();
    stdin.flush().unwrap();

    let mut reply = None;
    for _ in 0..64 {
        let line = recv(&mut stdout);
        if line.contains(r#""id":3"#) {
            reply = Some(line);
            break;
        }
    }
    let elapsed = start.elapsed();
    let reply = reply.expect("cancelled prompt reply");
    assert!(reply.contains(r#""stopReason":"cancelled""#), "{reply}");
    assert!(
        elapsed < Duration::from_millis(200),
        "cancel stopped in {elapsed:?}, under the 200ms budget"
    );

    drop(stdin);
    drop(child.wait());
}

/// session/new reports modes and config options; set_mode and
/// set_config_option reply with schema 1.5 bodies.
#[test]
fn test_acp_config_option_switch() {
    let (mut child, mut stdin, mut stdout) = spawn_server(&[]);
    let (sid, result) = handshake(&mut stdin, &mut stdout);
    assert!(result["modes"].is_object(), "modes present: {result}");
    assert!(
        result["configOptions"].is_array(),
        "config options present: {result}"
    );
    let current_model = result["configOptions"][0]["currentValue"]
        .as_str()
        .expect("model current value")
        .to_string();

    // set_mode replies with an empty body; the change rides a notification.
    let set_mode = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/set_mode",
        "params": {"sessionId": sid, "modeId": "manual"},
    });
    stdin.write_all(set_mode.to_string().as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    let mut mode_reply = None;
    let mut saw_mode_update = false;
    for _ in 0..16 {
        let line = recv(&mut stdout);
        if line.contains("current_mode_update") || line.contains("currentModeId") {
            saw_mode_update = true;
        }
        if line.contains(r#""id":4"#) {
            mode_reply = Some(line);
            break;
        }
    }
    assert!(saw_mode_update, "current_mode_update notification arrived");
    let mode_reply = mode_reply.expect("set_mode reply");
    assert!(
        mode_reply.contains(r#""result":{}"#),
        "set_mode replies with the empty body: {mode_reply}"
    );

    // set_config_option replies with configOptions wrapped in the response.
    let set_config = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "session/set_config_option",
        "params": {"sessionId": sid, "configId": "model", "value": current_model},
    });
    let reply = send_and_recv(&mut stdin, &mut stdout, &set_config.to_string());
    assert!(reply.contains(r#""configOptions""#), "{reply}");
    assert!(reply.contains(r#""currentValue""#), "{reply}");

    drop(stdin);
    drop(child.wait());
}

/// A one-mebibyte prompt arrives across many pipe fragments and a cancel
/// sent alongside it still stops the run, so the reader does not desync on a
/// large frame or lose the cancel behind it.
#[test]
fn test_acp_large_prompt_cancel() {
    let (mut child, mut stdin, mut stdout) = spawn_server(&[("HOUYICODER_ACP_STUB_HANG", "1")]);
    let (sid, _) = handshake(&mut stdin, &mut stdout);
    // The one mebibyte rides the opaque _meta map, not the model text: the
    // frame still arrives across many fragments without entering the model's
    // context budget, so the test isolates framing from context accounting.
    let big = "x".repeat(1024 * 1024);
    let prompt = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/prompt",
        "params": {
            "sessionId": sid,
            "prompt": [{"type": "text", "text": "hi"}],
            "_meta": {"blob": big},
        },
    })
    .to_string();
    let cancel =
        json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}});
    // Write the large frame in fragments so the reader reassembles it across
    // many reads, then the cancel immediately after.
    let bytes = format!("{prompt}\n{cancel}\n");
    for chunk in bytes.as_bytes().chunks(8192) {
        stdin.write_all(chunk).unwrap();
        stdin.flush().unwrap();
    }

    let mut reply = None;
    for _ in 0..256 {
        let line = recv(&mut stdout);
        if line.contains(r#""id":3"#) {
            reply = Some(line);
            break;
        }
    }
    let reply = reply.expect("prompt reply after a large frame");
    assert!(reply.contains(r#""stopReason":"cancelled""#), "{reply}");

    drop(stdin);
    drop(child.wait());
}

/// A prompt carrying an image block is rejected with a typed InvalidParams
/// rather than dropped silently.
#[test]
fn test_acp_rejects_image_block() {
    let (mut child, mut stdin, mut stdout) = spawn_server(&[]);
    let (sid, _) = handshake(&mut stdin, &mut stdout);
    let prompt = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "session/prompt",
        "params": {
            "sessionId": sid,
            "prompt": [{"type": "image", "data": "eA==", "mimeType": "image/png"}],
        },
    });
    let reply = send_and_recv(&mut stdin, &mut stdout, &prompt.to_string());
    assert!(reply.contains(r#""code":-32602"#), "{reply}");
    assert!(reply.contains("image content is not supported"), "{reply}");

    drop(stdin);
    drop(child.wait());
}
