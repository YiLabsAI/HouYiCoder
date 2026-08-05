//! Drop-in machine proof: a stock ACP client (a separate process) drives
//! our server process end-to-end over the real stdio socket. The verify
//! charter drove this by hand; here a test subprocess spawns the
//! acp_stdio binary, speaks raw JSON-RPC to its stdin, and asserts the
//! replies on its stdout — a regression-gated proof that we are a drop-in
//! ACP agent, not a fork. The session_id is read from stderr (the server
//! prints it on startup), so the prompt targets the real bound session.

#![cfg(feature = "acp-cross-decode")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Once;

/// Resolve the acp_stdio example binary path relative to this crate
/// manifest dir (the workspace target dir is two levels up).
fn binary_path() -> String {
    format!(
        "{}/../../target/debug/examples/acp_stdio",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Build the example once per test run if absent (cargo test does not
/// build examples by default).
fn ensure_built() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if !std::path::Path::new(&binary_path()).exists() {
            let status = Command::new("cargo")
                .args(["build", "--example", "acp_stdio"])
                .status()
                .expect("cargo build --example acp_stdio");
            assert!(status.success(), "build acp_stdio example");
        }
    });
}

/// Spawn the acp_stdio binary with piped stdio; return the bound
/// session_id (printed to stderr on startup).
fn spawn_server() -> (std::process::Child, String) {
    ensure_built();
    let mut child = Command::new(binary_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn acp_stdio");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read session_id");
    let session_id = line
        .trim()
        .strip_prefix("session_id=")
        .expect("server prints session_id=")
        .to_string();
    drop(reader);
    (child, session_id)
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
        if line.contains(want_id_fragment) {
            return line.to_string();
        }
    }
}

/// initialize replies with protocolVersion + agentCapabilities; then
/// session/prompt streams session/update + a PromptResponse with
/// stopReason end_turn. Proves a stock ACP client drives our server.
#[test]
fn test_drop_drives_init_prompt() {
    let (mut child, session_id) = spawn_server();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stdout = BufReader::new(stdout);

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let init_reply = send_and_recv(&mut stdin, &mut stdout, init, r#""id":1"#);
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
    let _ = child.wait();
}

/// An unknown method replies method-not-found (-32601) on the request id,
/// not null — the wire behaves as a stock ACP peer expects.
#[test]
fn test_drop_unknown_method_fails() {
    let (mut child, _session_id) = spawn_server();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stdout = BufReader::new(stdout);
    let bogus = r#"{"jsonrpc":"2.0","id":7,"method":"bogus","params":{}}"#;
    let reply = send_and_recv(&mut stdin, &mut stdout, bogus, r#""id":7"#);
    assert!(reply.contains(r#""code":-32601"#), "{reply}");
    drop(stdin);
    let _ = child.wait();
}

/// Malformed JSON replies ParseError (-32700) on the null id, per
/// JSON-RPC 2.0.
#[test]
fn test_drop_malformed_frame_fails() {
    let (mut child, _session_id) = spawn_server();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stdout = BufReader::new(stdout);
    let reply = send_and_recv(&mut stdin, &mut stdout, "not json at all", r#""id":null"#);
    assert!(reply.contains(r#""code":-32700"#), "{reply}");
    drop(stdin);
    let _ = child.wait();
}
