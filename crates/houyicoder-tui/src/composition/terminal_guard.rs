//! Terminal-mode scope for one TUI session.
//!
//! Raw input mode, the alternate screen, mouse reporting, bracketed paste,
//! focus reporting, and the hidden cursor are process-global terminal
//! changes. Without a single owner they leak on every exit that is not the
//! loop's normal return: an early error return, a panic unwinding past the
//! loop, or a setup failure partway through the mode list. The guard owns
//! both halves of the change, so a normal return, an error return, and an
//! unwind all restore the terminal exactly once.
//!
//! The sequence builders take the writer, so the emitted mode set and the
//! progress indicator are assertable without a terminal. Only the raw-mode
//! toggle and the two calls that write to the process terminal stay
//! terminal-bound.

use std::io::{self, Write, stdout};

use crossterm::{
    cursor::{Hide, SetCursorStyle, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

use crate::terminal_title;

/// The terminal-mode changes the guard makes on acquire and undoes on
/// release. This boundary exists because raw mode acts on the process's own
/// tty, which a test process does not own; the lifecycle contract is
/// otherwise only observable in a real terminal.
pub(crate) trait TerminalModes {
    /// Enter the modes. Called once, before any drawing.
    fn enter(&mut self) -> io::Result<()>;
    /// Restore the modes. Called once, by release or by drop.
    fn leave(&mut self) -> io::Result<()>;
}

/// The process terminal, through crossterm.
pub(crate) struct CrosstermModes;

impl TerminalModes for CrosstermModes {
    fn enter(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        enter_sequences(&mut stdout())
    }

    fn leave(&mut self) -> io::Result<()> {
        // Every step runs even when an earlier one fails: leaving the
        // alternate screen and showing the cursor again matters more than
        // reporting which step went wrong, so the restore never stops
        // halfway. The first failure is what the caller sees.
        restore_host_chrome();
        let raw = disable_raw_mode();
        let modes = leave_sequences(&mut stdout());
        raw.and(modes)
    }
}

/// Enter the alternate screen and the reporting modes. Parameterized on the
/// writer so the emitted sequence set is assertable without a terminal.
fn enter_sequences(out: &mut impl Write) -> io::Result<()> {
    execute!(out, EnterAlternateScreen, EnableBracketedPaste, Hide)?;
    execute!(out, EnableFocusChange)?;
    write_mouse_modes(out)
}

/// Leave the screen first, then clear the reporting modes and bring the
/// cursor back, so nothing is written into the alternate screen after it is
/// left.
fn leave_sequences(out: &mut impl Write) -> io::Result<()> {
    execute!(
        out,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        Show,
        SetCursorStyle::DefaultUserShape
    )
}

/// The four DECSET modes the TUI needs: button events, drag reporting, motion
/// reporting, and the SGR encoding that survives coordinates past 223.
const MOUSE_MODES: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h";

/// Enable SGR mouse reporting for clicks, drag, hover, and wheel events.
/// Reasserting these idempotent modes recovers from terminal state resets.
pub(crate) fn assert_mouse_modes() -> io::Result<()> {
    write_mouse_modes(&mut stdout())
}

fn write_mouse_modes(out: &mut impl Write) -> io::Result<()> {
    out.write_all(MOUSE_MODES)?;
    out.flush()
}

/// The indeterminate progress indicator, and the cleared form that stops it.
const PROGRESS_BUSY: &[u8] = b"\x1b]9;4;3;0\x1b\\";
const PROGRESS_CLEAR: &[u8] = b"\x1b]9;4;0;0\x1b\\";

/// Publish the progress indicator through OSC 9;4.
pub(crate) fn set_terminal_progress(busy: bool) -> io::Result<()> {
    write_progress(&mut stdout(), busy)
}

fn write_progress(out: &mut impl Write, busy: bool) -> io::Result<()> {
    out.write_all(if busy { PROGRESS_BUSY } else { PROGRESS_CLEAR })?;
    out.flush()
}

/// Clear the progress indicator on the way out. Written unconditionally: an
/// exit while a run is busy would otherwise leave the indicator running in
/// the host chrome, and a terminal that does not know OSC 9;4 ignores it.
fn clear_terminal_progress(out: &mut impl Write) {
    if let Err(e) = write_progress(out, false) {
        tracing::warn!(error = %e, "progress indicator not cleared");
    }
}

/// Restore the terminal chrome that outlives the alternate screen. Both steps
/// report their own failures, so a teardown never stops halfway.
fn restore_host_chrome() {
    terminal_title::restore();
    clear_terminal_progress(&mut stdout());
}

/// Owns the TUI's terminal modes for the session's lifetime. An explicit
/// release runs on the normal path; drop runs on every other path. One flag
/// keeps the two from both running the restore.
pub(crate) struct TerminalGuard<M: TerminalModes = CrosstermModes> {
    modes: M,
    released: bool,
}

impl<M: TerminalModes> TerminalGuard<M> {
    /// Enter the TUI terminal modes and return the owner that will restore
    /// them. A setup failure partway through the list is undone before the
    /// error returns, so a half-entered terminal is never left behind.
    /// Undoing a mode the setup never reached is harmless: each leave step is
    /// a no-op when its mode is already off.
    pub(crate) fn acquire(mut modes: M) -> io::Result<Self> {
        if let Err(e) = modes.enter() {
            if let Err(restore) = modes.leave() {
                tracing::warn!(error = %restore, "terminal restore after failed setup did not finish");
            }
            return Err(e);
        }
        Ok(Self {
            modes,
            released: false,
        })
    }

    /// Restore the terminal on the normal exit path. Idempotent with drop, so
    /// a later unwind still cannot restore twice.
    pub(crate) fn release(&mut self) {
        self.release_once();
    }

    /// The single restore door: the normal return, the error return, and a
    /// panic unwinding past the loop all reach it. A failed restore is
    /// reported and swallowed -- an error here must not replace the failure
    /// already propagating, and a panic from drop during an unwind aborts the
    /// process.
    fn release_once(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Err(e) = self.modes.leave() {
            tracing::warn!(error = %e, "terminal restore did not finish");
        }
    }
}

impl<M: TerminalModes> Drop for TerminalGuard<M> {
    fn drop(&mut self) {
        self.release_once();
    }
}

#[cfg(test)]
mod tests;
