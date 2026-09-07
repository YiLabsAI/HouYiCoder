//! Shared pseudo-terminal harness for real-binary interaction tests.
//! It isolates process state, sends key events, and captures terminal output.

#![allow(dead_code)] // shared helpers vary by integration target

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::cmdbuilder::CommandBuilder;
use portable_pty::{Child, PtySize};

/// A real TUI process with writable input and accumulated terminal output.
/// Drop stops the child; the reader exits when the pseudo-terminal closes.
pub struct PtySession {
    _child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
    _reader: thread::JoinHandle<()>,
    sessions_dir: PathBuf,
    /// A temp home the harness created (not caller-provided); cleaned in Drop
    /// so isolated config roots do not accumulate in /tmp.
    _owned_home: Option<PathBuf>,
}

const ROWS: u16 = 24;
/// Wide viewport for stable path and layout assertions.
const COLS: u16 = 200;

#[derive(Clone, Copy)]
struct LaunchOptions {
    rows: u16,
    pretrust: bool,
}

impl PtySession {
    /// Launch the real binary with an isolated home, stub provider, and workspace cwd.
    pub fn launch() -> Self {
        Self::launch_inner(None, None, None, None)
    }

    /// Launch from an explicit working directory.
    pub fn launch_in_dir(dir: PathBuf) -> Self {
        Self::launch_inner(None, None, None, Some(dir))
    }

    /// Slow stub streaming so tests can act during a live run.
    pub fn launch_with_stub_delay(ms: u64) -> Self {
        Self::launch_inner(None, Some(ms), None, None)
    }

    /// Like launch_with_stub_delay, but runs in the given repo dir instead of
    /// the workspace root (isolated startup, no project state to delay the
    /// stub).
    pub fn launch_in_repo_with_delay(repo: PathBuf, ms: u64) -> Self {
        Self::launch_inner(None, Some(ms), None, Some(repo))
    }

    /// Drive the stub provider with a JSON sequence of model output items.
    pub fn launch_with_stub_script(script_json: &str) -> Self {
        Self::launch_inner(Some(script_json.to_string()), None, None, None)
    }

    /// Launch with a caller-owned home for config and memory assertions.
    pub fn launch_with_home(home: PathBuf) -> Self {
        Self::launch_inner(None, None, Some(home), None)
    }

    /// Launch without pre-trusting the workspace so startup trust UX can be tested.
    pub fn launch_untrusted(home: PathBuf, cwd: PathBuf) -> Self {
        let sessions_dir = fresh_temp_dir("sessions");
        Self::launch_impl(
            None,
            None,
            Some(home),
            Some(cwd),
            &[],
            sessions_dir,
            LaunchOptions {
                rows: ROWS,
                pretrust: false,
            },
        )
    }

    /// Run a scripted session in a caller-owned repository fixture.
    pub fn launch_in_repo_with_script(repo: PathBuf, script_json: &str) -> Self {
        Self::launch_inner(Some(script_json.to_string()), None, None, Some(repo))
    }

    fn launch_inner(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<PathBuf>,
        cwd_override: Option<PathBuf>,
    ) -> Self {
        Self::launch_with_args(script, delay, home, cwd_override, &[])
    }

    /// Launch with explicit CLI arguments while retaining test isolation.
    pub fn launch_with_args(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<PathBuf>,
        cwd_override: Option<PathBuf>,
        extra_args: &[String],
    ) -> Self {
        let sessions_dir = fresh_temp_dir("sessions");
        Self::launch_with_sessions_dir(script, delay, home, cwd_override, extra_args, sessions_dir)
    }

    /// Launch with a shared session root for multi-process and resume tests.
    pub fn launch_with_sessions_dir(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<PathBuf>,
        cwd_override: Option<PathBuf>,
        extra_args: &[String],
        sessions_dir: PathBuf,
    ) -> Self {
        Self::launch_impl(
            script,
            delay,
            home,
            cwd_override,
            extra_args,
            sessions_dir,
            LaunchOptions {
                rows: ROWS,
                pretrust: true,
            },
        )
    }

    /// Launch with an explicit terminal height.
    pub fn launch_with_sessions_dir_rows(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<PathBuf>,
        cwd_override: Option<PathBuf>,
        extra_args: &[String],
        sessions_dir: PathBuf,
        rows: u16,
    ) -> Self {
        Self::launch_impl(
            script,
            delay,
            home,
            cwd_override,
            extra_args,
            sessions_dir,
            LaunchOptions {
                rows,
                pretrust: true,
            },
        )
    }

    fn launch_impl(
        script: Option<String>,
        delay: Option<u64>,
        home: Option<PathBuf>,
        cwd_override: Option<PathBuf>,
        extra_args: &[String],
        sessions_dir: PathBuf,
        options: LaunchOptions,
    ) -> Self {
        let bin = houyi_binary_path();
        let cwd = cwd_override.unwrap_or_else(workspace_root);
        // Default to a harness-owned home and pre-trust ordinary fixtures.
        let home_provided = home.is_some();
        let home = home.unwrap_or_else(|| fresh_temp_dir("pty-home"));
        let _owned_home = if home_provided {
            None
        } else {
            Some(home.clone())
        };
        let mut cmd = CommandBuilder::new(&bin);
        cmd.cwd(&cwd);
        for arg in extra_args {
            cmd.arg(arg);
        }
        cmd.env("HOME", &home);
        // Override both home inputs so ambient config cannot enter the fixture.
        cmd.env("HOUYICODER_CONFIG_HOME", home.join(".houyicoder"));
        let settings = home.join(".houyicoder").join("settings.json");
        if options.pretrust {
            houyicoder_config::persist_project_trust(&settings, &cwd)
                .expect("pre-trust cwd in temp settings");
        }
        // Keep durable session output inside the per-launch fixture.
        cmd.env("HOUYICODER_SESSIONS_DIR", &sessions_dir);
        // Empty every supported key so tests deterministically select the stub
        // provider and never inherit network credentials.
        cmd.env("DASHSCOPE_API_KEY", "");
        cmd.env("OPENAI_API_KEY", "");
        cmd.env("HOUYICODER_API_KEY", "");
        // Keep unrelated fence diagnostics out of transcript assertions.
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
                rows: options.rows,
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
        let output = Arc::new(Mutex::new(Vec::<u8>::with_capacity(64 * 1024)));
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
            _owned_home,
        }
    }

    /// Return the isolated session-log root.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Write raw bytes to the PTY master (the binary reads them as crossterm
    /// key events on stdin).
    pub fn send_bytes(&mut self, bytes: &[u8]) {
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

    /// Kill the process immediately for crash-recovery tests.
    pub fn kill_hard(&mut self) {
        drop(self._child.kill());
    }

    /// Wait until the child exits.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self._child.try_wait().ok().flatten().is_some() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// The accumulated raw ANSI output as a lossy UTF-8 string.
    pub fn output(&self) -> String {
        let bytes = self.output.lock().expect("output lock").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Return accumulated output without terminal escape sequences.
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

    /// Return plain output without whitespace for style-independent matching.
    pub fn output_compact(&self) -> String {
        self.output_plain()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    /// Poll until the compact (whitespace-stripped) output contains marker,
    /// or timeout elapses. The marker itself must be whitespace-free so it
    /// matches the compacted form (e.g. "ctrl+otoexpand").
    pub fn wait_for_compact(&mut self, marker: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.output_compact().contains(marker) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.output_compact().contains(marker)
    }

    /// Clear accumulated bytes before an assertion about the current render.
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
        // Kill the process group so spawned descendants cannot outlive the fixture.
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
        if let Some(h) = &self._owned_home {
            drop(fs::remove_dir_all(h));
        }
    }
}

/// Terminal key encodings used by interaction journeys.
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
    /// Shift+Up / Shift+Down — the fleet-pill row selection keys. Crossterm
    /// emits these as CSI sequences with the Shift modifier byte (2).
    ShiftUp,
    ShiftDown,
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
            // Shift-modified arrows: CSI ... ;2 <final>. The binary's crossterm
            // parser decodes the modifier byte (2 = Shift) + KeyEventModifiers.
            Key::ShiftUp => b"\x1b[1;2A".to_vec(),
            Key::ShiftDown => b"\x1b[1;2B".to_vec(),
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
pub fn fresh_temp_dir(slug: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // Retry on collision: a stale leftover dir from a previous run (same pid
    // recycled by the OS, since nextest gives each test its own process so
    // the per-process SEQ restarts at 0) makes create_dir fail with
    // AlreadyExists. Increment n until a free slot lands. This is the root
    // fix for the parallel-run flake --retries used to paper over.
    loop {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = env::temp_dir().join(format!("houyi-ui-{slug}-{}-{n}", process::id(),));
        match fs::create_dir(&p) {
            Ok(()) => return p,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("mkdir temp dir {p:?}: {e}"),
        }
    }
}

/// Seed a throwaway git repo the binary can run in (a workspace manifest so
/// resolve_project_workspace pins the dir). Isolates the project-scope
/// memory root from the developer's real workspace so list scans are not
/// polluted by real entries the test did not save.
#[allow(clippy::disallowed_methods)]
pub fn make_temp_repo(slug: &str) -> PathBuf {
    let dir = fresh_temp_dir(&format!("repo-{slug}"));
    fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("write manifest");
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@x"][..],
        &["config", "user.name", "t"][..],
        &["add", "Cargo.toml"][..],
        &["commit", "-m", "init", "-q"][..],
    ] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {:?}", args);
    }
    dir
}

/// A one-response stub script: plain text only, so a run completes in one
/// step (no tool call, no approval pause). Shared by the resume + status
/// PTY tests. The outer array is the per-call list -- the provider parses
/// Vec<Vec<OutputItem>>, so a bare [{...}] does not parse and the stub
/// silently fails to engage.
pub const ONE_REPLY_SCRIPT: &str = r#"[[{"type":"Text","text":"logged"}]]"#;

/// Write a fixture export file (a legacy-ULID session id + a model + two
/// durable events) the resume path can deserialize. serde ignores the
/// derived-stats fields a full export carries, so this slice round-trips
/// through resume. Shared by the resume + status-provenance PTY tests.
pub fn write_resume_fixture() -> PathBuf {
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
    let dir = env::temp_dir().join(format!(
        "houyi-resume-fixture-{}-{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&dir).expect("mkdir fixture dir");
    let path = dir.join("export.json");
    fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).expect("write fixture");
    path
}

mod seed;
#[allow(unused_imports)]
pub use seed::*;

/// List the session-id dirs (each a sid directory) under a sessions root.
/// Files (the export json, lock files) are filtered out. Shared by the
/// live-export-resume PTY test.
pub fn sid_dirs(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .unwrap_or_else(|e| panic!("read sessions root {root:?}: {e}"))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect()
}

/// Launch the binary + pick local mode (no login, no network ever) → land on
/// the Working screen. Shared by every test.
pub fn pty_session() -> PtySession {
    pty_session_inner(PtySession::launch())
}

/// Like pty_session(), but runs the binary in the given dir instead
/// of the workspace root. For tests that assert on the workspace
/// additional-dirs list's empty state: from a linked worktree the startup
/// allow-back adds the main checkout's git dir, so the workspace root's list
/// is never empty.
pub fn pty_session_in_dir(dir: PathBuf) -> PtySession {
    pty_session_inner(PtySession::launch_in_dir(dir))
}

/// Like pty_session, but the stub streams with an inter-chunk delay so
/// a run stays in-flight long enough to drive mid-run keys (dynamic mode
/// switch, mid-run abort, etc.).
pub fn pty_session_slow(ms: u64) -> PtySession {
    pty_session_inner(PtySession::launch_with_stub_delay(ms))
}

/// Like pty_session_slow, but in an isolated temp repo (avoids the
/// workspace root's project state delaying stub delivery past the timeout).
pub fn pty_session_slow_in_repo(repo: PathBuf, ms: u64) -> PtySession {
    pty_session_inner(PtySession::launch_in_repo_with_delay(repo, ms))
}

/// Like pty_session, but overrides HOME so the memory roots + settings
/// file land in a temp dir the test owns, AND seeds a throwaway git repo as
/// the working directory so the project-scope memory root (the workspace
/// cwd's memory dir) is also temp — not the developer's real workspace, whose
/// entries would leak into the test's list_memories scan.
pub fn pty_session_isolated(home: PathBuf) -> PtySession {
    let repo = make_temp_repo("home");
    pty_session_inner(PtySession::launch_with_args(
        None,
        None,
        Some(home),
        Some(repo),
        &[],
    ))
}

/// Like pty_session, but the stub emits a scripted response sequence
/// (HOUYICODER_STUB_SCRIPT) so the run drives real tool calls. Used by the
/// tool-call + permission-flow tests.
pub fn pty_session_scripted(script_json: &str) -> PtySession {
    pty_session_inner(PtySession::launch_with_stub_script(script_json))
}

/// Like pty_session_scripted, but with a custom PTY row count. The
/// /status pane caps at area/2; a 24-row terminal clips the lower fields, so
/// tests that assert on breaker / provenance / tokens / tasks need a taller
/// terminal to admit the full field set.
pub fn pty_session_scripted_rows(script_json: &str, rows: u16) -> PtySession {
    let sessions_dir = fresh_temp_dir("sessions");
    pty_session_inner(PtySession::launch_with_sessions_dir_rows(
        Some(script_json.to_string()),
        None,
        None,
        None,
        &[],
        sessions_dir,
        rows,
    ))
}

/// Like pty_session_scripted, but the binary runs in the given repo
/// dir (a throwaway git repo) instead of the workspace root. Used by the
/// worktree PTY tests so a real linked worktree is created + removed under the
/// temp repo, never the developer workspace. The caller seeds the repo (init +
/// one commit + a workspace manifest) before launching.
pub fn pty_session_in_repo(repo: PathBuf, script_json: &str) -> PtySession {
    pty_session_inner(PtySession::launch_in_repo_with_script(repo, script_json))
}

/// Like pty_session_in_repo, but with a custom HOME so the test can
/// populate .claude/skills/ (ecosystem path) before launch.
pub fn pty_session_with_home(repo: PathBuf, home: PathBuf, script_json: &str) -> PtySession {
    pty_session_inner(PtySession::launch_with_args(
        Some(script_json.to_string()),
        None,
        Some(home),
        Some(repo),
        &[],
    ))
}

/// Like pty_session_scripted, but the stub streams with an
/// inter-chunk delay so a run stays in-flight long enough to drive mid-run
/// keys (Esc interrupt, recall). Used by the multi-agent Esc tests.
pub fn pty_session_slow_scripted(ms: u64, script_json: &str) -> PtySession {
    let sessions_dir = fresh_temp_dir("sessions");
    pty_session_inner(PtySession::launch_with_sessions_dir(
        Some(script_json.to_string()),
        Some(ms),
        None,
        None,
        &[],
        sessions_dir,
    ))
}

fn pty_session_inner(mut s: PtySession) -> PtySession {
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

/// Invoke a skill by typing @skill:name + Enter (the skill activation prefix).
pub fn run_skill_command(s: &mut PtySession, name: &str) {
    s.send_str("@skill:");
    s.send_str(name);
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
