//! Drop-in machine proof: a stock ACP client (a separate process) drives
//! our server process end-to-end over the real stdio socket. This binary
//! re-runs itself as the server child, speaks raw JSON-RPC to its stdin, and
//! asserts the replies on its stdout, a regression-gated proof that we are a
//! drop-in ACP agent, not a fork. The session id comes from the session/new
//! reply, the same way a stock client learns it.
//!
//! The binary runs harness-free, so the child's stdout carries only protocol
//! frames.

#![allow(clippy::disallowed_methods)] // test scaffolding spawns this binary, not an engine call

#[path = "common/acp_child_fixture.rs"]
mod acp_child_fixture;
mod common;

use crate::common::child_command;

use serde_json::Value;
use serde_json::json;
use std::env::current_dir;
use std::env::var_os;
use std::io::{BufRead, BufReader, Write};
use std::process::Child;
use std::process::ChildStdin;
use std::process::ChildStdout;

/// Spawn the acp_stdio binary with piped stdio, run the initialize
/// handshake, then learn the bound session id from the session/new reply.
/// The initialize reply comes back with the handles so a test can assert on
/// the handshake the server already performed.
fn spawn_server() -> (Child, ChildStdin, BufReader<ChildStdout>, String, String) {
    let mut child = child_command(acp_child_fixture::SERVER_ENV)
        .spawn()
        .expect("spawn the ACP server child");
    let mut stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut stdout = BufReader::new(stdout);

    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    let init_reply = send_and_recv(&mut stdin, &mut stdout, &init.to_string(), r#""id":1"#);

    let cwd = current_dir().expect("current dir");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {"cwd": cwd.to_string_lossy(), "mcpServers": []},
    });
    let reply = send_and_recv(&mut stdin, &mut stdout, &request.to_string(), r#""id":2"#);
    let value: Value = serde_json::from_str(&reply).expect("session/new reply parses");
    let session_id = value["result"]["sessionId"]
        .as_str()
        .expect("session/new reports the session id")
        .to_string();
    (child, stdin, stdout, session_id, init_reply)
}

/// Send one NDJSON line to stdin, return the next stdout frame that
/// carries the wanted id fragment (skipping session/update
/// notifications in between).
fn send_and_recv(
    stdin: &mut impl Write,
    stdout: &mut impl BufRead,
    frame: &str,
    want_id_fragment: &str,
) -> String {
    stdin.write_all(frame.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    loop {
        let mut line = String::new();
        let n = stdout.read_line(&mut line).expect("read reply");
        assert!(n > 0, "server closed stdout waiting for {want_id_fragment}");
        let line = line.trim();
        // Every stdout line is one frame: a blank line here means the writer
        // emitted a second terminator and the wire no longer frames cleanly.
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("stdout line is not a JSON frame ({e}): {line:?}"));
        if line.contains(want_id_fragment) {
            return line.to_string();
        }
    }
}

/// initialize replies with protocolVersion + agentCapabilities; then
/// session/prompt streams session/update + a PromptResponse with
/// stopReason end_turn. Proves a stock ACP client drives our server.
fn test_drop_capabilities_and_prompt() {
    let (mut child, mut stdin, mut stdout, session_id, init_reply) = spawn_server();

    assert!(
        init_reply.contains(r#""protocolVersion":1"#),
        "initialize reply: {init_reply}"
    );
    assert!(
        init_reply.contains(r#""agentCapabilities""#),
        "initialize reply: {init_reply}"
    );

    let prompt = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{{"sessionId":"{sid}","prompt":[{{"type":"text","text":"hi"}}]}}}}"#,
        sid = session_id
    );
    let prompt_reply = send_and_recv(&mut stdin, &mut stdout, &prompt, r#""id":2"#);
    assert!(
        prompt_reply.contains(r#""stopReason":"end_turn""#),
        "prompt reply: {prompt_reply}"
    );

    drop(stdin);
    drop(child.wait());
}

/// An unknown method replies method-not-found (-32601) on the request id,
/// not null — the wire behaves as a stock ACP peer expects.
fn test_drop_unknown_method_fails() {
    let (mut child, mut stdin, mut stdout, _session_id, _init_reply) = spawn_server();
    let bogus = r#"{"jsonrpc":"2.0","id":7,"method":"bogus","params":{}}"#;
    let reply = send_and_recv(&mut stdin, &mut stdout, bogus, r#""id":7"#);
    assert!(reply.contains(r#""code":-32601"#), "{reply}");
    drop(stdin);
    drop(child.wait());
}

/// Malformed JSON replies ParseError (-32700) on the null id, per
/// JSON-RPC 2.0.
fn test_drop_malformed_frame_fails() {
    let (mut child, mut stdin, mut stdout, _session_id, _init_reply) = spawn_server();
    let reply = send_and_recv(&mut stdin, &mut stdout, "not json at all", r#""id":null"#);
    assert!(reply.contains(r#""code":-32700"#), "{reply}");
    drop(stdin);
    drop(child.wait());
}

/// The tests, run by hand because the binary doubles as its own child.
fn main() {
    if var_os(acp_child_fixture::SERVER_ENV).is_some() {
        acp_child_fixture::serve_stdio_server();
        return;
    }
    common::run_tests(&[
        (
            "test_drop_capabilities_and_prompt",
            test_drop_capabilities_and_prompt,
        ),
        (
            "test_drop_unknown_method_fails",
            test_drop_unknown_method_fails,
        ),
        (
            "test_drop_malformed_frame_fails",
            test_drop_malformed_frame_fails,
        ),
    ]);
}
