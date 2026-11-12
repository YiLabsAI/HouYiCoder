//! The terminal boundary a desktop session runs against: a hangup probe, a
//! one-time termination registration, and the output capabilities a renderer
//! gates its escape sequences on. The layer above owns event reading and
//! frame drawing; this module owns what those depend on.

use std::env::var;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

#[cfg(unix)]
use crate::unix::terminal::{probe_hangup, watch_signals};
#[cfg(not(unix))]
pub use crate::windows::terminal::read_console_event;
#[cfg(not(unix))]
use crate::windows::terminal::{ConsoleGuard, console_gone, input_is_pipe, watch_close_event};

/// The begin marker of a synchronized output frame.
pub const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
/// The end marker of a synchronized output frame.
pub const SYNC_END: &[u8] = b"\x1b[?2026l";

/// The terminal's output capabilities, so a caller that gates an escape
/// sequence on them can explain its choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalCaps {
    /// The terminal accepts synchronized output frames: a frame bracketed
    /// with the begin and end markers is presented as one update, so a
    /// partial frame never shows.
    pub sync_output: bool,
    /// The terminal accepts the progress indicator escape, a terminal-chrome
    /// feature not every terminal implements. A terminal that never said it
    /// understands the sequence ignores the bytes, so they are worth writing
    /// only for one that did.
    pub progress_indicator: bool,
}

impl TerminalCaps {
    /// The capabilities of the terminal this process runs in, decided once.
    /// A terminal that identifies itself through its environment variable is
    /// reported as capable; a terminal under a multiplexer is not, because
    /// the multiplexer owns the outer presentation.
    #[must_use]
    pub fn detect() -> Self {
        static CAPS: OnceLock<TerminalCaps> = OnceLock::new();
        *CAPS.get_or_init(|| {
            caps_for(
                env_str("TERM_PROGRAM"),
                env_str("TMUX").filter(|value| !value.is_empty()),
                env_str("WT_SESSION"),
                env_str("WezTerm"),
                env_str("GHOSTTY_RESOURCES_DIR"),
            )
        })
    }
}

/// The environment variable read as a String, or None when unset.
fn env_str(name: &str) -> Option<String> {
    var(name).ok()
}

/// The synchronized-output decision over the identifying variables a process
/// carries. Separated from the environment read so every branch is testable
/// without the process environment polluting the result.
fn sync_output_for(
    program: Option<String>,
    multiplexer: Option<String>,
    wt_session: Option<String>,
    wezterm: Option<String>,
    ghostty: Option<String>,
) -> bool {
    if multiplexer.is_some() {
        return false;
    }
    // The terminals that accept the synchronized-frame protocol, by the
    // direct variable each one sets.
    let sync_terminals = [wt_session, wezterm, ghostty]
        .into_iter()
        .any(|v| v.is_some_and(|value| !value.is_empty()));
    if sync_terminals {
        return true;
    }
    let Some(program) = program else {
        return false;
    };
    const KNOWN: [&str; 7] = [
        "WezTerm",
        "ghostty",
        "iTerm.app",
        "vscode",
        "kitty",
        "kitty.app",
        "alacritty",
    ];
    KNOWN
        .into_iter()
        .any(|name| program.eq_ignore_ascii_case(name))
}

/// The progress-indicator decision over the identifying variables a process
/// carries. The OSC 9;4 progress sequence is a terminal-chrome feature
/// not every terminal implements, so it is gated on the terminals that
/// do, not on the sync-frame set: Windows Terminal owns its own progress
/// surface and does not need the indicator written through the stream.
fn progress_indicator_for(
    program: Option<String>,
    wezterm: Option<String>,
    ghostty: Option<String>,
) -> bool {
    let sync_terminals = [wezterm, ghostty]
        .into_iter()
        .any(|v| v.is_some_and(|value| !value.is_empty()));
    if sync_terminals {
        return true;
    }
    let Some(program) = program else {
        return false;
    };
    const KNOWN: [&str; 2] = ["iTerm.app", "ghostty"];
    KNOWN
        .into_iter()
        .any(|name| program.eq_ignore_ascii_case(name))
}

/// The capabilities the test environment reports for the process. Separated
/// from detect() so the progress indicator can be asserted independently.
pub(crate) fn caps_for(
    program: Option<String>,
    multiplexer: Option<String>,
    wt_session: Option<String>,
    wezterm: Option<String>,
    ghostty: Option<String>,
) -> TerminalCaps {
    TerminalCaps {
        sync_output: sync_output_for(
            program.clone(),
            multiplexer,
            wt_session,
            wezterm.clone(),
            ghostty.clone(),
        ),
        progress_indicator: progress_indicator_for(program, wezterm, ghostty),
    }
}

/// Whether the process's terminal has gone away. The answer is a probe, not
/// a signal: a terminal that closes leaves its descriptor readable with
/// nothing to read, and only the descriptor shows that state. A platform
/// whose console reports closure through an event instead answers false and
/// the event carries the news.
pub fn terminal_gone() -> bool {
    #[cfg(unix)]
    {
        probe_hangup()
    }
    #[cfg(not(unix))]
    {
        console_gone()
    }
}

/// The termination signal state for this process, registered once.
///
/// Registration is process-wide and cannot be undone cheaply: a second
/// registration would leave a handler nobody reads. A session switch runs
/// the loop again and must read the same flag the first registration
/// raised, so the watch is shared rather than rebuilt.
pub struct TerminationWatch {
    requested: Arc<AtomicBool>,
}

impl TerminationWatch {
    /// Whether a termination signal has arrived.
    #[must_use]
    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    /// Whether the run loop should leave: the user asked to quit, or the
    /// platform asked the process to stop, and either way the loop leaves
    /// through the teardown that restores the terminal.
    #[must_use]
    pub fn should_exit(&self, quit: bool) -> bool {
        quit || self.requested()
    }

    /// A watch in a stated state, for a caller testing one path without
    /// raising a real signal.
    #[must_use]
    pub fn for_test(seen_signal: bool) -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(seen_signal)),
        }
    }
}

/// Watch the events that end a session without unwinding. The watch lives
/// for the whole run; the registration is process-wide and stays installed,
/// which is correct for a process that serves one session at a time. The
/// wake runs when the platform raises the flag, so the loop it feeds leaves
/// within one poll interval of the event.
pub fn watch_termination(
    wake: impl Fn() + Send + Sync + 'static,
) -> std::io::Result<TerminationWatch> {
    static REQUESTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    let requested = REQUESTED.get_or_init(|| Arc::new(AtomicBool::new(false)));
    install_watch(Arc::clone(requested), wake)?;
    Ok(TerminationWatch {
        requested: Arc::clone(requested),
    })
}

/// Install the platform watch, raising the shared flag when the platform
/// ends the session. A platform with nothing to watch installs nothing, so
/// its flag is never raised by the platform.
fn install_watch(
    requested: Arc<AtomicBool>,
    wake: impl Fn() + Send + Sync + 'static,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        watch_signals(requested, wake)
    }
    #[cfg(not(unix))]
    {
        watch_close_event(requested, wake)
    }
}

/// The console mode owner for the session, or None on a platform whose
/// console has no modes to own. The owner is the single active one: the
/// modes are process-global, and a second concurrent owner is refused.
/// Dropping it restores the modes the console started with.
pub fn console_owner() -> Option<ConsoleOwner> {
    ConsoleOwner::take()
}

/// A platform-agnostic handle on the console mode ownership. Dropping the
/// owner restores the console modes it took; a platform with no console
/// modes holds nothing.
pub struct ConsoleOwner {
    /// Held for its restore-on-drop, never read: the guard's own release is
    /// the whole ownership contract.
    #[cfg(not(unix))]
    #[expect(dead_code, reason = "held for its restore-on-drop side effect")]
    guard: ConsoleGuard,
}

impl ConsoleOwner {
    #[cfg(not(unix))]
    fn take() -> Option<Self> {
        ConsoleGuard::acquire().map(|guard| Self { guard }).ok()
    }

    #[cfg(unix)]
    fn take() -> Option<Self> {
        None
    }
}

/// Whether the process's input arrives as a byte stream rather than as
/// console records. A terminal window that hands the process a pipe answers
/// no console API, so the reader switches to the byte decoder; a terminal
/// that identifies itself through its environment and feeds a pipe is the
/// case this answers.
pub fn pipe_input() -> bool {
    if !pipe_terminal_environment() {
        return false;
    }
    #[cfg(unix)]
    {
        false
    }
    #[cfg(not(unix))]
    {
        input_is_pipe()
    }
}

/// Whether the platform delivers a clipboard paste as a run of keystrokes
/// rather than as a single paste event. The console API splits a paste
/// into one record per keystroke, so the reader folds the run; a unix
/// terminal reports its own paste event, so no folding is needed there.
pub fn paste_arrives_as_keystrokes() -> bool {
    cfg!(target_os = "windows")
}

/// True when the environment says the terminal is one that hands the
/// process a pipe: the terminal class variable and a shell environment are
/// both set. The pair keeps a plain redirected stdin from switching the
/// reader on a platform that also reports pipes.
fn pipe_terminal_environment() -> bool {
    let msys = var("MSYSTEM").is_ok_and(|value| !value.is_empty());
    let term = var("TERM").is_ok_and(|value| !value.is_empty());
    msys || term
}

#[cfg(test)]
mod tests;

pub mod vt;
