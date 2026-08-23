//! Shared PTY test harness for the TUI: spawn the real houyi binary under a
//! pseudo-terminal, feed it keystrokes, and assert on the rendered byte stream.
//!
//! This is the "real UX path" layer that TestBackend unit tests cannot cover:
//! the actual crossterm event loop, the real repaint, and the full key-routing
//! chain from a terminal. It complements the inline unit tests (fast, exhaustive,
//! assert on App state) — it does not replace them. The unit layer catches
//! cell-state regressions (e.g. the Workspace cursor-clamp bug, via
//! render_buffer); this layer catches flow + render + key-routing breakage
//! that only surfaces when the real binary drives a real terminal.
//!
//! Tests are #[ignore] (they spawn a binary + a PTY — too slow + flaky for the
//! 60s commit gate). Run via make test ui (which builds the bin first) or
//! cargo test --test ui_<category> -- --ignored after cargo build --bin houyi.
//!
//! This module is compiled into EVERY ui_* test binary (each does mod common;),
//! so a helper used by one category but not another would warn dead_code — the
//! module-level allow keeps the shared helpers clean across categories.
//!
//! Assertion strategy: accumulate the raw ANSI bytes the binary writes, and
//! assert by substring (wait_for, assert_contains). This sidesteps a full
//! terminal emulator (a fragile 150-line vte Perform would make every test
//! flaky on an exotic escape). A complete screen-grid emulator can land later
//! if cell-precise assertions are needed here; for now the raw-stream +
//! SGR-proximity checks cover the flow/render class.

#![allow(dead_code)] // test fixtures; used by some ui test binaries, unused from others

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::cmdbuilder::CommandBuilder;
use portable_pty::{Child, PtySize};

/// A live houyi TUI driven through a PTY. The reader thread continuously
/// drains the PTY master into an accumulated byte buffer; wait_for polls
/// that buffer for a marker substring. Drop kills the child + waits on it
/// (best-effort, never panics); the reader thread is detached + exits on
/// its own when the master EOFs.
pub struct PtySession {
    _child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn std::io::Write + Send>,
    output: Arc<std::sync::Mutex<Vec<u8>>>,
    _reader: thread::JoinHandle<()>,
    sessions_dir: std::path::PathBuf,
}

const ROWS: u16 = 24;
/// Wide enough that long canonical paths (macOS /private/var/folders/.../basename)
/// render fully instead of truncating, so substring assertions on the basename
/// match. The TUI layout adapts to any width.
const COLS: u16 = 200;

impl PtySession {
    /// Spawn target/debug/houyi under a 200×24 PTY with no API key
    /// (FakeProvider) + the workspace root as cwd (so the project manifest
    /// resolves cleanly). Returns once the binary has started; the reader
    /// thread is already accumulating output.
    pub fn launch() -> Self {
        Self::launch_inner(None, None, None, None)
    }

    /// Like launch(), but runs the binary in the given dir instead of the
    /// workspace root. For tests that assert on the workspace additional-dirs
    /// list's empty state: launched from a linked worktree (the default
    /// workspace root during a sprint), the startup allow-back adds the main
    /// checkout's git dir, so the list is never empty there.
    pub fn launch_in_dir(dir: std::path::PathBuf) -> Self {
        Self::launch_inner(None, None, None, Some(dir))
    }

    /// Like launch(), but sets HOUYICODER_STUB_DELAY_MS so the stub run streams
    /// slowly enough to drive mid-run keys (e.g. a Shift+Tab mode cycle while
    /// agent_busy). The default launch() streams back-to-back, so its busy
    /// window is too short to catch mid-run.
    pub fn launch_with_stub_delay(ms: u64) -> Self {
        Self::launch_inner(None, Some(ms), None, None)
    }

    /// Like launch_with_stub_delay, but runs in the given repo dir instead of
    /// the workspace root (isolated startup, no project state to delay the
    /// stub).
    pub fn launch_in_repo_with_delay(repo: std::path::PathBuf, ms: u64) -> Self {
        Self::launch_inner(None, Some(ms), None, Some(repo))
    }

    /// Like launch(), but sets HOUYICODER_STUB_SCRIPT so the stub emits a
    /// scripted response sequence (a ToolCall then plain text) so PTY tests
    /// can drive real tool calls (glob / read / edit / todo_write) through the
    /// real binary — the interaction layer (permission cards, tool-result
    /// rendering, transcript fold) is otherwise unreachable. The script is a
    /// JSON array of per-call output-item lists; see provider_or_stub.
    pub fn launch_with_stub_script(script_json: &str) -> Self {
        Self::launch_inner(Some(script_json.to_string()), None, None, None)
    }

    /// Like launch(), but overrides HOME so the user + auto memory roots and
    /// the settings file land in a temp dir the test owns + can assert on,
    /// never touching the developer's real home. The project-scope root still
    /// lives under the workspace cwd; /save writes to the auto scope (the last
    /// root), so it lands in the temp HOME. Used by the /memory smoke tests.
    pub fn launch_with_home(home: std::path::PathBuf) -> Self {
        Self::launch_inner(None, None, Some(home), None)
    }

    /// Like launch_with_stub_script, but runs the binary in the given repo
    /// dir instead of the workspace root. Used by the worktree PTY tests so
    /// enter_worktree creates worktrees in a throwaway git repo (a real
    /// linked worktree under the repo state dir), never in the developer
    /// actual workspace. The dir must carry a workspace manifest so
    /// resolve_project_workspace pins it and the worktree controller wires;
    /// git must be init'd with one commit so branching from HEAD succeeds.
    pub fn launch_in_repo_with_script(repo: std::path::PathBuf, script_json: &str) -> Self {
        Self::launch_inner(Some(script_json.to_string()), None, None, Some(repo))
    }

    fn launch_inner(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<std::path::PathBuf>,
        cwd_override: Option<std::path::PathBuf>,
    ) -> Self {
        Self::launch_with_args(script, delay, home, cwd_override, &[])
    }

    /// Like launch(), but passes extra args to the binary (used by the
    /// --resume tests to spawn the binary with a --resume <file> flag).
    /// Otherwise identical isolation (sessions dir isolated, stub mode,
    /// no network).
    pub fn launch_with_args(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<std::path::PathBuf>,
        cwd_override: Option<std::path::PathBuf>,
        extra_args: &[String],
    ) -> Self {
        let sessions_dir = fresh_temp_dir("sessions");
        Self::launch_with_sessions_dir(script, delay, home, cwd_override, extra_args, sessions_dir)
    }

    /// Like launch_with_args, but uses a caller-provided sessions dir instead
    /// of a fresh temp dir. Used by tests that need to share the sessions root
    /// across two binary spawns (e.g. the lock-contention test where a second
    /// --resume <sid> must see the same session the first holds the lock on),
    /// or that need to seed a session on disk before launch (the in-process
    /// swap test writes a fixture session into the dir the binary will read).
    pub fn launch_with_sessions_dir(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<std::path::PathBuf>,
        cwd_override: Option<std::path::PathBuf>,
        extra_args: &[String],
        sessions_dir: std::path::PathBuf,
    ) -> Self {
        Self::launch_impl(
            script,
            delay,
            home,
            cwd_override,
            extra_args,
            sessions_dir,
            ROWS,
        )
    }

    /// Like launch_with_sessions_dir, but with a custom PTY row count. Used by
    /// the /status pane tests: the pane caps at area/2, so a 24-row terminal
    /// clips the lower status fields (breaker / provenance / tokens / tasks).
    /// A taller terminal admits the full field set.
    pub fn launch_with_sessions_dir_rows(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<std::path::PathBuf>,
        cwd_override: Option<std::path::PathBuf>,
        extra_args: &[String],
        sessions_dir: std::path::PathBuf,
        rows: u16,
    ) -> Self {
        Self::launch_impl(
            script,
            delay,
            home,
            cwd_override,
            extra_args,
            sessions_dir,
            rows,
        )
    }

    fn launch_impl(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<std::path::PathBuf>,
        cwd_override: Option<std::path::PathBuf>,
        extra_args: &[String],
        sessions_dir: std::path::PathBuf,
        rows: u16,
    ) -> Self {
        let bin = houyi_binary_path();
        let cwd = cwd_override.unwrap_or_else(workspace_root);
        let mut cmd = CommandBuilder::new(&bin);
        cmd.cwd(cwd);
        for arg in extra_args {
            cmd.arg(arg);
        }
        if let Some(h) = home {
            cmd.env("HOME", &h);
            // config_home() checks HOUYICODER_CONFIG_HOME BEFORE HOME, so an
            // ambient value in the developer's shell leaks into the subprocess
            // and the server writes settings.json to the wrong path. Point it
            // at the temp home so the test owns the config root regardless.
            cmd.env("HOUYICODER_CONFIG_HOME", h.join(".houyicoder"));
        }
        // Isolate the session log root: the production binary now persists
        // every durable event to a file backend at the sessions root, so
        // without this override each PTY test would write its session log
        // into the developer real home. Point the override at a per-launch
        // temp dir so every test's log lands somewhere it owns + cleans up.
        cmd.env("HOUYICODER_SESSIONS_DIR", &sessions_dir);
        // Force stub mode: set the API keys to EMPTY (not just removed). The
        // config layer treats an empty key as missing (resolve_api_key filters
        // !is_empty, checking DASHSCOPE / OPENAI / HOUYICODER in order), so
        // build_provider falls to the stub path. The binary does not auto-load
        // .env (dotenvy was removed; settings.json + env are the only sources),
        // so no stray .env in the worktree can revive a real provider + hit the
        // network. All THREE key vars must be emptied — missing one
        // (HOUYICODER_API_KEY) re-enables a real provider. This matters now
        // that the dynamic mode-switch test sends
        // a MessageSend — a real provider would make a network call mid-test.
        cmd.env("DASHSCOPE_API_KEY", "");
        cmd.env("OPENAI_API_KEY", "");
        cmd.env("HOUYICODER_API_KEY", "");
        // Suppress the fence-status startup notice so it does not occupy a
        // transcript line that PTY text assertions must account for. The
        // notice is a user-facing safety alert; PTY tests run unfenced by
        // design (stub sandbox) and assert on run/command output, not fence
        // status.
        cmd.env("HOUYICODER_QUIET_FENCE", "1");
        if let Some(s) = script {
            cmd.env("HOUYICODER_STUB_SCRIPT", s);
        }
        if let Some(ms) = delay {
            cmd.env("HOUYICODER_STUB_DELAY_MS", ms.to_string());
        }

        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");
        let child = pair.slave.spawn_command(cmd).unwrap_or_else(|e| {
            panic!("spawn {bin:?}: {e}\n  did you run cargo build --bin houyi?")
        });
        // Drop the slave so reads return EOF when the child exits.
        drop(pair.slave);
        let writer = pair.master.take_writer().expect("pty writer");
        let reader = pair.master.try_clone_reader().expect("pty reader clone");
        let output = Arc::new(std::sync::Mutex::new(Vec::<u8>::with_capacity(64 * 1024)));
        let out_buf = output.clone();
        let reader_thread = thread::spawn(move || {
            let mut r = reader;
            let mut buf = [0u8; 4096];
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut o) = out_buf.lock() {
                            o.extend_from_slice(&buf[..n]);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            _child: child,
            writer,
            output,
            _reader: reader_thread,
            sessions_dir,
        }
    }

    /// The per-launch temp dir the binary writes session logs into (set via
    /// the sessions-dir env override so PTY tests never touch the developer
    /// real home). After a turn, <sid>/log.jsonl lives here.
    pub fn sessions_dir(&self) -> &std::path::Path {
        &self.sessions_dir
    }

    /// Write raw bytes to the PTY master (the binary reads them as crossterm
    /// key events on stdin).
    pub fn send_bytes(&mut self, bytes: &[u8]) {
        use std::io::Write;
        self.writer.write_all(bytes).expect("pty write");
        self.writer.flush().expect("pty flush");
    }

    /// Type a printable string (one Char event per char).
    pub fn send_str(&mut self, s: &str) {
        self.send_bytes(s.as_bytes());
    }

    /// Send a crossterm-style key. Covers the keys the /permissions flows use.
    pub fn send_key(&mut self, key: &Key) {
        let bytes = key.encode();
        self.send_bytes(&bytes);
    }

    /// Hard-kill the child (SIGKILL on Unix) so the process dies without
    /// running Drop handlers. Used by the crash-release test to verify the
    /// OS releases the advisory file flock on process death (the single-
    /// writer invariant must hold even when a holder crashes, not just on a
    /// clean exit). Best-effort: ignores a kill error (process may be gone).
    pub fn kill_hard(&mut self) {
        drop(self._child.kill());
    }

    /// The accumulated raw ANSI output as a lossy UTF-8 string.
    pub fn output(&self) -> String {
        let bytes = self.output.lock().expect("output lock").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The accumulated output with ANSI escape sequences stripped, so a
    /// substring assertion can match text that the renderer splits across
    /// styled spans (e.g. "Auto-memory: on" where the value is a separate
    /// span and an SGR run sits between the label and the value). Raw bytes
    /// are kept for the few assertions that need SGR proximity; this is the
    /// form for content checks.
    pub fn output_plain(&self) -> String {
        strip_ansi(&self.output())
    }

    /// Poll until marker appears in the PLAIN (ANSI-stripped) output, or
    /// timeout elapses. Use this when the marker would cross a styled-span
    /// boundary in the raw stream (see output_plain).
    pub fn wait_for_plain(&mut self, marker: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.output_plain().contains(marker) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.output_plain().contains(marker)
    }

    /// Drop the accumulated output so a subsequent absence check (wait_for
    /// returning false) tests the CURRENT render, not the historical one.
    /// Needed because the buffer accumulates every byte ever written — a
    /// marker from an earlier render would otherwise always read present.
    #[allow(dead_code)]
    pub fn clear_output(&mut self) {
        if let Ok(mut o) = self.output.lock() {
            o.clear();
        }
    }

    /// Poll until marker appears in the output, or timeout elapses.
    /// Returns true on hit. Replaces sleeps — deterministic on the render
    /// arriving, not on a fixed delay.
    pub fn wait_for(&mut self, marker: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.output().contains(marker) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.output().contains(marker)
    }

    /// Panic with the current screen dump if marker is not in the output.
    pub fn assert_contains(&self, marker: &str) {
        let out = self.output();
        assert!(
            out.contains(marker),
            "expected {marker:?} in the rendered output.\n--- screen dump ---\n{out}"
        );
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort kill + wait; never panic in Drop. The PTY child is a
        // session leader (forkpty setsid), so SIGKILL its whole process group
        // to reap descendants the binary spawned (git, ps, sandbox-exec) that
        // a SIGHUP-only kill on the leader would orphan — nextest flags those
        // as leaks. kill with a negative pid targets the process group.
        let _kill = self._child.kill();
        #[cfg(unix)]
        {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;
            if let Some(pid) = self._child.process_id() {
                let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
            }
        }
        let _wait = self._child.wait();
    }
}

/// A crossterm-compatible key encoding for the subset the flows need.
/// A crossterm-compatible key encoding for the subset the flows need. Some
/// variants are not used by the first tests but are part of the harness API
/// for future flows (Esc to exit sub-modes, Up/Down to move the cursor, etc.).
#[allow(dead_code)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Esc,
    Tab,
    Backtab,
    Backspace,
    Left,
    Right,
    Up,
    Down,
}

impl Key {
    fn encode(&self) -> Vec<u8> {
        match self {
            Key::Char(c) => {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
            // Ctrl+<c> encodes as the control byte c & 0x1f (Ctrl+G = 0x07).
            Key::Ctrl(c) => vec![(*c as u8) & 0x1f],
            Key::Enter => b"\r".to_vec(),
            Key::Esc => b"\x1b".to_vec(),
            Key::Tab => b"\t".to_vec(),
            Key::Backtab => b"\x1b[Z".to_vec(),
            Key::Backspace => b"\x7f".to_vec(),
            // Application arrows — crossterm emits CSI with a trailing modifier
            // byte. The bare CSI form (no modifier) is what an unmodified
            // arrow key produces; the binary's crossterm parser accepts either.
            Key::Left => b"\x1b[D".to_vec(),
            Key::Right => b"\x1b[C".to_vec(),
            Key::Up => b"\x1b[A".to_vec(),
            Key::Down => b"\x1b[B".to_vec(),
        }
    }
}

/// Resolve target/debug/houyi relative to this crate's manifest dir
/// (the workspace root is two levels above the TUI crate dir).
fn houyi_binary_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("target").join("debug").join("houyi"))
        .unwrap_or_else(|| PathBuf::from("target/debug/houyi"))
}

/// The workspace root (two levels above this crate). Used as the binary's cwd
/// so resolve_project_workspace walks up to the workspace Cargo.toml.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Default timeout for a render to arrive after a keystroke. Generous (the
/// binary redraws in well under 100ms; 2s absorbs a cold-cache first render
/// + any PTY scheduling latency).
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(6);

/// Strip ANSI escape sequences from a string for content assertions. Covers
/// CSI (ESC [ ... final byte 0x40-0x7E), OSC (ESC ] ... BEL or ST), and bare
/// two-byte escapes (ESC + next). Anything else passes through. A full vte
/// parser is overkill for substring checks; this is enough for the styled
/// spans the renderer emits (SGR color runs).
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // ESC ... — drop the escape sequence.
        if i + 1 >= bytes.len() {
            break;
        }
        match bytes[i + 1] {
            b'[' => {
                // CSI: skip until a final byte 0x40-0x7E.
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                i += 1; // consume the final byte
            }
            b']' => {
                // OSC: skip until BEL (0x07) or ST (ESC \).
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                // Bare two-byte escape (e.g. ESC c, ESC =). Drop both.
                i += 2;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- shared flow helpers (used by every ui_* category binary) ----

/// A unique real temp directory for one test run. The server's
/// add_working_dir canonicalizes + checks is_dir, so the added path must
/// exist as a directory; a non-existent path is rejected silently (a real
/// UX gap surfaced by the workspace tests). Uniqueness comes from an
/// atomic counter, not the wall clock: the clock resolution on some
/// platforms is coarse enough that two parallel test launches in the same
/// process can land the same nanosecond stamp and collide on mkdir, which
/// A per-launch temp dir under the system temp root. Each PTY test gets its
/// own process under nextest, so a process-local counter would reset to 0 in
/// every test; combined with the OS recycling a pid, a new test process could
/// mint the same path as a stale leftover dir from a prior run, and create_dir
/// (which fails on an existing dir) would panic. The pid + a per-process
/// monotonic counter cannot.
pub fn fresh_temp_dir(slug: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // Retry on collision: a stale leftover dir from a previous run (same pid
    // recycled by the OS, since nextest gives each test its own process so
    // the per-process SEQ restarts at 0) makes create_dir fail with
    // AlreadyExists. Increment n until a free slot lands. This is the root
    // fix for the parallel-run flake --retries used to paper over.
    loop {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("houyi-ui-{slug}-{}-{n}", std::process::id(),));
        match std::fs::create_dir(&p) {
            Ok(()) => return p,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("mkdir temp dir {p:?}: {e}"),
        }
    }
}

/// A one-response stub script: plain text only, so a run completes in one
/// step (no tool call, no approval pause). Shared by the resume + status
/// PTY tests.
pub const ONE_REPLY_SCRIPT: &str = r#"[{"type":"Text","text":"logged"}]"#;

/// Write a fixture export file (a legacy-ULID session id + a model + two
/// durable events) the resume path can deserialize. serde ignores the
/// derived-stats fields a full export carries, so this slice round-trips
/// through resume. Shared by the resume + status-provenance PTY tests.
pub fn write_resume_fixture() -> std::path::PathBuf {
    use houyicoder_core::{EventId, SessionId, TurnEvent, TurnEventKind};
    let legacy_sid = "01KZ5RDH4DG6YV0EDBX1KSKTRA"; // legacy ULID (pre-change)
    let sid = SessionId::from_display_string(legacy_sid).expect("legacy ULID parses");
    let mk = |kind: TurnEventKind| TurnEvent {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        kind,
    };
    let events = vec![
        mk(TurnEventKind::UserInput {
            text: "resumed hello from export".into(),
        }),
        mk(TurnEventKind::AssistantMessage {
            text: "resumed reply from export".into(),
            thinking: None,
        }),
    ];
    let doc = serde_json::json!({
        "session_id": legacy_sid,
        "model": "stub-resume-model",
        "trajectory": events,
    });
    let dir = std::env::temp_dir().join(format!(
        "houyi-resume-fixture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir fixture dir");
    let path = dir.join("export.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).expect("write fixture");
    path
}

mod seed;
#[allow(unused_imports)]
pub use seed::*;

/// List the session-id dirs (each a sid directory) under a sessions root.
/// Files (the export json, lock files) are filtered out. Shared by the
/// live-export-resume PTY test.
pub fn sid_dirs(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("read sessions root {root:?}: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// Launch the binary + pick local mode (no login, no network ever) → land on
/// the Working screen. Shared by every test.
pub fn session_on_working() -> PtySession {
    session_on_working_inner(PtySession::launch())
}

/// Like session_on_working(), but runs the binary in the given dir instead
/// of the workspace root. For tests that assert on the workspace
/// additional-dirs list's empty state: from a linked worktree the startup
/// allow-back adds the main checkout's git dir, so the workspace root's list
/// is never empty.
pub fn session_on_working_in_dir(dir: std::path::PathBuf) -> PtySession {
    session_on_working_inner(PtySession::launch_in_dir(dir))
}

/// Like session_on_working, but the stub streams with an inter-chunk delay so
/// a run stays in-flight long enough to drive mid-run keys (dynamic mode
/// switch, mid-run abort, etc.).
pub fn session_on_working_slow(ms: u64) -> PtySession {
    session_on_working_inner(PtySession::launch_with_stub_delay(ms))
}

/// Like session_on_working_slow, but in an isolated temp repo (avoids the
/// workspace root's project state delaying stub delivery past the timeout).
pub fn session_on_working_slow_in_repo(repo: std::path::PathBuf, ms: u64) -> PtySession {
    session_on_working_inner(PtySession::launch_in_repo_with_delay(repo, ms))
}

/// Like session_on_working, but overrides HOME so the memory roots + settings
/// file land in a temp dir the test owns. Used by the /memory smoke tests so
/// /save + /memory toggle never touch the developer's real home.
pub fn session_on_working_with_home(home: std::path::PathBuf) -> PtySession {
    session_on_working_inner(PtySession::launch_with_home(home))
}

/// Like session_on_working, but the stub emits a scripted response sequence
/// (HOUYICODER_STUB_SCRIPT) so the run drives real tool calls. Used by the
/// tool-call + permission-flow tests.
pub fn session_on_working_with_script(script_json: &str) -> PtySession {
    session_on_working_inner(PtySession::launch_with_stub_script(script_json))
}

/// Like session_on_working_with_script, but with a custom PTY row count. The
/// /status pane caps at area/2; a 24-row terminal clips the lower fields, so
/// tests that assert on breaker / provenance / tokens / tasks need a taller
/// terminal to admit the full field set.
pub fn session_on_working_with_script_rows(script_json: &str, rows: u16) -> PtySession {
    let sessions_dir = fresh_temp_dir("sessions");
    session_on_working_inner(PtySession::launch_with_sessions_dir_rows(
        Some(script_json.to_string()),
        None,
        None,
        None,
        &[],
        sessions_dir,
        rows,
    ))
}

/// Like session_on_working_with_script, but the binary runs in the given repo
/// dir (a throwaway git repo) instead of the workspace root. Used by the
/// worktree PTY tests so a real linked worktree is created + removed under the
/// temp repo, never the developer workspace. The caller seeds the repo (init +
/// one commit + a workspace manifest) before launching.
pub fn session_on_working_in_repo(repo: std::path::PathBuf, script_json: &str) -> PtySession {
    session_on_working_inner(PtySession::launch_in_repo_with_script(repo, script_json))
}

fn session_on_working_inner(mut s: PtySession) -> PtySession {
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "login screen should render; raw output: {:?}",
        s.output()
    );
    // '3' = local mode: skips auth, no network call even if a message is sent.
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen should render after local login"
    );
    s
}

/// Run a slash command (with optional args) through the real palette path.
/// The palette accepts ascii-graphic chars + the space separator, so an
/// arg-taking command like "permissions git off" lands in the query as-is.
/// When the spaced query matches no palette entry, Enter falls through to
/// the raw-submit branch + ships the typed query as a slash command — so
/// arg-taking local commands are reachable end-to-end.
pub fn run_slash_command(s: &mut PtySession, cmd: &str) {
    s.send_key(&Key::Char('/'));
    s.send_str(cmd);
    s.send_key(&Key::Enter);
}

/// Open the /permissions pane via the slash palette (the real path).
pub fn open_permissions(s: &mut PtySession) {
    s.send_key(&Key::Char('/'));
    s.send_str("permissions");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("Permissions:", RENDER_TIMEOUT),
        "pane header should render"
    );
}

/// Tab to Workspace (default tab is Allow; 3 Rights → Workspace).
pub fn tab_to_workspace(s: &mut PtySession) {
    for _ in 0..3 {
        s.send_key(&Key::Right);
    }
    assert!(
        s.wait_for("[Workspace]", RENDER_TIMEOUT),
        "Workspace tab should be the focused one"
    );
}
