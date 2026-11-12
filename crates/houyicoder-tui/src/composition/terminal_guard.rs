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
//!
//! Every write to the process terminal goes through an ownership gate. Mouse
//! reporting, the progress indicator, and the tab title are process-global
//! state, and they are only ever correct while a guard holds the modes. A
//! caller that never acquired the guard — a unit test that dispatches a
//! resize or focus event on a bare app, most of all — would otherwise put the
//! terminal it happens to run in into mouse-reporting mode, and no guard
//! would be left to turn it back off. The guard itself sets and clears the
//! flag, so a test over a recording fake exercises the gate without a tty.

use std::io::{self, Write, stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::{
    cursor::{Hide, SetCursorStyle, Show},
    event::{DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use houyicoder_platform::desktop::terminal::ConsoleOwner;
use houyicoder_platform::desktop::terminal::TerminalCaps;
use houyicoder_platform::desktop::terminal::console_owner;
use houyicoder_platform::desktop::terminal::pipe_input;

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
#[derive(Default)]
pub(crate) struct CrosstermModes {
    /// The console mode ownership, taken on enter and released on leave. A
    /// platform with no console modes holds None for the whole session.
    console: Option<ConsoleOwner>,
}

/// Whether this process currently owns the terminal's modes. Set when the
/// guard acquires the modes and cleared when it releases them, so a write to
/// the process terminal can tell a live session from a caller that never took
/// the modes.
static TERMINAL_MODES_HELD: AtomicBool = AtomicBool::new(false);

/// Whether this process currently owns the terminal's modes.
pub(crate) fn terminal_modes_held() -> bool {
    TERMINAL_MODES_HELD.load(Ordering::SeqCst)
}

impl TerminalModes for CrosstermModes {
    fn enter(&mut self) -> io::Result<()> {
        // The console mode ownership is taken first so the quick-edit clear
        // holds before any mode the reader depends on: a click during setup
        // must not freeze the reader the modes are for. A platform with no
        // console modes answers None and this step is a no-op.
        self.console = console_owner();
        // A terminal that hands the process a pipe answers no console API,
        // so the raw-mode toggle would error on a handle it cannot read.
        // Skip it there; the escape sequences still go to the stream.
        if !pipe_input() {
            enable_raw_mode()?;
        }
        enter_sequences(&mut stdout())
    }

    fn leave(&mut self) -> io::Result<()> {
        // Every step runs even when an earlier one fails: leaving the
        // alternate screen and showing the cursor again matters more than
        // reporting which step went wrong, so the restore never stops
        // halfway. The first failure is what the caller sees.
        restore_host_chrome();
        let raw = if pipe_input() {
            Ok(())
        } else {
            disable_raw_mode()
        };
        let modes = leave_sequences(&mut stdout());
        // The console modes go back last: the quick-edit clear the platform
        // applied is what kept the reader alive through the steps above.
        self.console = None;
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
/// left. The mouse report clear is written as raw DECSET resets, mirroring
/// the enter path: the DisableMouseCapture command routes through the
/// Windows console API on that platform, which errors when mouse capture was
/// never enabled through that API. This code enables mouse reporting through
/// raw escapes, so the leave path clears them the same way, keeping the
/// sequence builder assertable on any writer without console state.
fn leave_sequences(out: &mut impl Write) -> io::Result<()> {
    execute!(
        out,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableFocusChange,
        Show,
        SetCursorStyle::DefaultUserShape
    )?;
    out.write_all(MOUSE_MODES_CLEAR)?;
    out.flush()
}

/// The four DECSET modes the TUI needs: button events, drag reporting, motion
/// reporting, and the SGR encoding that survives coordinates past 223.
const MOUSE_MODES: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h";

/// The inverse of MOUSE_MODES, turning each report mode back off. Raw escapes
/// mirror the enable path so the clear works on any writer and any platform.
const MOUSE_MODES_CLEAR: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";

/// Enable SGR mouse reporting for clicks, drag, hover, and wheel events.
/// Reasserting these idempotent modes recovers from terminal state resets.
///
/// A caller that never took the modes gets nothing written. The sequence would
/// put the caller's own terminal into mouse-reporting mode, and no guard would
/// be left to turn it back off, so every motion over that window would print a
/// coordinate report into whatever reads the stream next.
pub(crate) fn assert_mouse_modes() -> io::Result<()> {
    write_mouse_modes_if_held(&mut stdout(), terminal_modes_held())
}

fn write_mouse_modes_if_held(out: &mut impl Write, held: bool) -> io::Result<()> {
    if !held {
        return Ok(());
    }
    write_mouse_modes(out)
}

fn write_mouse_modes(out: &mut impl Write) -> io::Result<()> {
    out.write_all(MOUSE_MODES)?;
    out.flush()
}

/// The indeterminate progress indicator, and the cleared form that stops it.
const PROGRESS_BUSY: &[u8] = b"\x1b]9;4;3;0\x1b\\";
const PROGRESS_CLEAR: &[u8] = b"\x1b]9;4;0;0\x1b\\";

/// Publish the progress indicator through OSC 9;4. Gated on the same ownership
/// as the mouse modes: an indicator published by a caller that never took the
/// modes would keep spinning in a terminal no session is driving. Gated on the
/// terminal's capabilities too: the indicator is a terminal-chrome feature,
/// and a terminal that never said it understands the sequence ignores it, so
/// the bytes only ride the stream of one that does.
pub(crate) fn set_terminal_progress(busy: bool) -> io::Result<()> {
    write_progress_if_held(&mut stdout(), busy, terminal_modes_held())
}

fn write_progress_if_held(out: &mut impl Write, busy: bool, held: bool) -> io::Result<()> {
    if !held || !TerminalCaps::detect().progress_indicator {
        return Ok(());
    }
    write_progress(out, busy)
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
///
/// Gated on ownership: a caller that never took the modes has no chrome to
/// restore, and retitling the terminal it happens to run in would be a write
/// no session asked for.
fn restore_host_chrome() {
    if !terminal_modes_held() {
        return;
    }
    terminal_title::restore();
    clear_terminal_progress(&mut stdout());
}

/// Restore the terminal from alternate screen and raw mode on fatal panic, so
/// the panic message prints on the primary screen.
pub fn restore_terminal_for_panic() {
    disable_raw_mode().ok();
    let mut out = stdout();
    drop(execute!(out, LeaveAlternateScreen, Show));
    restore_host_chrome();
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
        // The modes are now this process's to restore, so the chrome that
        // mirrors them may write while the guard holds them.
        TERMINAL_MODES_HELD.store(true, Ordering::SeqCst);
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
        // Cleared after the leave path: the chrome restore inside leave still
        // runs under ownership, and a later caller sees no terminal held.
        TERMINAL_MODES_HELD.store(false, Ordering::SeqCst);
    }
}

impl<M: TerminalModes> Drop for TerminalGuard<M> {
    fn drop(&mut self) {
        self.release_once();
    }
}

#[cfg(test)]
mod tests;
