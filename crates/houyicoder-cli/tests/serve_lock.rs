//! The detached server holds its session's lock for as long as it serves.

#![cfg(unix)]
#![expect(
    clippy::disallowed_methods,
    reason = "the test starts the binary under test"
)]

use std::env::temp_dir;
use std::fs::OpenOptions;
use std::fs::TryLockError;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::process::id;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::channel;
use std::thread::sleep;
use std::thread::spawn;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use houyicoder_context::SessionId;
use houyicoder_context::session_class::LOCK_FILE;
use houyicoder_context::session_class::session_dir;

/// The first launch of a freshly linked binary can stall before main while
/// the platform verifies it, so the deadline is generous.
const START_TIMEOUT: Duration = Duration::from_secs(60);

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = temp_dir().join(format!("houyi-{tag}-{}-{nanos}", id()));
    remove_dir_all(&root).ok();
    create_dir_all(&root).expect("mkdir the root");
    root
}

/// The server records its session for its whole life, so the session reads as
/// locked while it runs and free once it exits. A background prompt pass in a
/// later launch takes that same lock before writing a record, and without it
/// the pass would revert the record the server keeps current.
///
/// The freed lock after the server exits is the control: it proves the probe
/// reads this process's lock file rather than failing for another reason.
#[test]
#[ignore]
fn test_serve_holds_session_lock() {
    let root = temp_root("serve-lock");
    let socket = root.join("s.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_houyi"))
        .arg("--serve")
        .arg(&socket)
        .env("HOME", &root)
        .env("HOUYICODER_SESSIONS_DIR", &root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the server");
    let mut server = ServerProcess(child);
    let sid = read_session_id(&mut server.0);
    let lock_path = session_dir(&root, sid).join(LOCK_FILE);
    wait_for_file(&lock_path);
    assert!(
        matches!(try_lock(&lock_path), Err(TryLockError::WouldBlock)),
        "the serving process holds its session"
    );
    server.stop();
    assert!(
        try_lock(&lock_path).is_ok(),
        "the lock frees when the server exits"
    );
    remove_dir_all(&root).ok();
}

/// The running server, stopped when the test ends however it ends.
struct ServerProcess(Child);

impl ServerProcess {
    fn stop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The session id the server prints before it serves.
///
/// The read runs on its own thread so a server that stalls before printing
/// cannot hold the suite open: the deadline turns a startup failure into a
/// failed assertion that names what the server printed. The thread ends once
/// the test stops receiving, since its next send finds the channel closed.
fn read_session_id(child: &mut Child) -> SessionId {
    let stderr = child.stderr.take().expect("the server's stderr");
    let (line_tx, line_rx) = channel::<String>();
    let _reader = spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + START_TIMEOUT;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match line_rx.recv_timeout(left) {
            Ok(line) => {
                if let Some(rest) = line.strip_prefix("session_id=") {
                    return SessionId::from_display_string(rest.trim()).expect("the id parses");
                }
                seen.push(line);
            }
            Err(RecvTimeoutError::Timeout) => {
                panic!("the server printed no session id within the deadline: {seen:?}");
            }
            Err(RecvTimeoutError::Disconnected) => {
                let exited = child.try_wait().ok().flatten();
                panic!("the server printed no session id (exit {exited:?}): {seen:?}");
            }
        }
    }
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + START_TIMEOUT;
    while !path.is_file() {
        assert!(Instant::now() < deadline, "no lock file at {path:?}");
        sleep(Duration::from_millis(20));
    }
}

/// Take the lock if it is free, or report why it is not.
fn try_lock(path: &Path) -> Result<(), TryLockError> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .expect("open the lock file");
    file.try_lock()
}
