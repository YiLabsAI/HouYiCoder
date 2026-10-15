use std::cell::Cell;
use std::io::{self, Error, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use super::*;

/// Records the enter and leave calls the guard makes, and can fail either one
/// so the setup and teardown failure paths are exercised without a terminal.
struct RecordingModes {
    enters: Rc<Cell<u32>>,
    leaves: Rc<Cell<u32>>,
    fail_enter: bool,
    fail_leave: bool,
}

impl TerminalModes for RecordingModes {
    fn enter(&mut self) -> io::Result<()> {
        self.enters.set(self.enters.get() + 1);
        if self.fail_enter {
            return Err(Error::other("enter failed"));
        }
        Ok(())
    }

    fn leave(&mut self) -> io::Result<()> {
        self.leaves.set(self.leaves.get() + 1);
        if self.fail_leave {
            return Err(Error::other("leave failed"));
        }
        Ok(())
    }
}

fn modes(fail_enter: bool, fail_leave: bool) -> (RecordingModes, Rc<Cell<u32>>, Rc<Cell<u32>>) {
    let enters = Rc::new(Cell::new(0));
    let leaves = Rc::new(Cell::new(0));
    let modes = RecordingModes {
        enters: Rc::clone(&enters),
        leaves: Rc::clone(&leaves),
        fail_enter,
        fail_leave,
    };
    (modes, enters, leaves)
}

#[test]
fn test_acquire_enters_once() {
    let (m, enters, leaves) = modes(false, false);
    let mut guard = TerminalGuard::acquire(m).expect("acquire");
    assert_eq!(enters.get(), 1, "enter runs once on acquire");
    assert_eq!(leaves.get(), 0, "acquire does not restore");
    guard.release();
    assert_eq!(leaves.get(), 1, "release restores");
}

#[test]
fn test_drop_restores_once() {
    let (m, _enters, leaves) = modes(false, false);
    {
        let _guard = TerminalGuard::acquire(m).expect("acquire");
        assert_eq!(leaves.get(), 0, "no restore while the guard is held");
    }
    assert_eq!(leaves.get(), 1, "drop restores the terminal");
}

#[test]
fn test_release_then_drop_restores() {
    let (m, _enters, leaves) = modes(false, false);
    let mut guard = TerminalGuard::acquire(m).expect("acquire");
    guard.release();
    assert_eq!(leaves.get(), 1, "release restores");
    drop(guard);
    assert_eq!(leaves.get(), 1, "drop must not restore a second time");
}

/// The failure this guard exists for: a panic unwinding past the run loop
/// still restores the terminal. Without the drop, the console stays in raw
/// mode on the alternate screen after the process prints its panic.
#[test]
fn test_unwind_restores() {
    let (m, _enters, leaves) = modes(false, false);
    let unwind = catch_unwind(AssertUnwindSafe(|| {
        let _guard = TerminalGuard::acquire(m).expect("acquire");
        panic!("run loop failed");
    }));
    assert!(unwind.is_err(), "the panic was captured");
    assert_eq!(leaves.get(), 1, "the unwind restored the terminal");
}

/// An early error return is the other path that used to leak: a failure
/// between acquire and the loop's normal teardown.
#[test]
fn test_error_return_restores() {
    let (m, _enters, leaves) = modes(false, false);
    let run = move || -> io::Result<()> {
        let _guard = TerminalGuard::acquire(m)?;
        Err(Error::other("draw failed"))
    };
    assert!(run().is_err(), "the error propagated");
    assert_eq!(leaves.get(), 1, "the error return restored the terminal");
}

/// A setup failure partway through the mode list must not leave the modes it
/// did reach behind.
#[test]
fn test_failed_setup_undoes_enter() {
    let (m, enters, leaves) = modes(true, false);
    let err = TerminalGuard::acquire(m);
    assert!(err.is_err(), "a failed setup returns the error");
    assert_eq!(enters.get(), 1, "enter was attempted");
    assert_eq!(leaves.get(), 1, "the partial enter was undone");
}

/// A restore that itself fails must not panic: a panic from drop during an
/// unwind aborts the process, which is strictly worse than the unrestored
/// terminal.
#[test]
fn test_failed_restore_is_swallowed() {
    let (m, _enters, leaves) = modes(false, true);
    {
        let _guard = TerminalGuard::acquire(m).expect("acquire");
    }
    assert_eq!(leaves.get(), 1, "the restore was attempted and swallowed");
}

/// The four mouse reporting modes the TUI depends on. A dropped mode is a
/// silent capability loss: clicks, drags, hover, or coordinates past column
/// 223 stop arriving with no other symptom.
#[test]
fn test_mouse_modes_enabled() {
    let mut buf: Vec<u8> = Vec::new();
    write_mouse_modes(&mut buf).expect("write");
    let text = String::from_utf8(buf).expect("ascii escapes");
    for mode in ["1000", "1002", "1003", "1006"] {
        assert!(
            text.contains(&format!("?{mode}h")),
            "mode {mode} enabled: {text:?}"
        );
    }
}

/// The modes the enter sequence turns on. These DECSET numbers are the
/// terminal protocol contract, not an encoding detail: dropping one silently
/// loses the alternate screen, paste bracketing, the hidden cursor, focus
/// reporting, or mouse events.
#[test]
fn test_enter_sequences_set_modes() {
    let mut buf: Vec<u8> = Vec::new();
    enter_sequences(&mut buf).expect("write");
    let text = String::from_utf8(buf).expect("ascii escapes");
    for mode in ["1049", "2004", "1004"] {
        assert!(text.contains(&format!("?{mode}h")), "mode {mode}: {text:?}");
    }
    assert!(text.contains("?25l"), "cursor hidden: {text:?}");
}

/// The leave sequence has to undo exactly what the enter sequence turned on,
/// including the cursor shape and the mouse capture that the raw DECSET
/// writes in the enter path do not own.
#[test]
fn test_leave_sequences_clear_modes() {
    let mut buf: Vec<u8> = Vec::new();
    leave_sequences(&mut buf).expect("write");
    let text = String::from_utf8(buf).expect("ascii escapes");
    for mode in ["1049", "2004", "1004"] {
        assert!(text.contains(&format!("?{mode}l")), "mode {mode}: {text:?}");
    }
    assert!(text.contains("?25h"), "cursor shown again: {text:?}");
    for mode in ["1000", "1002", "1003", "1006"] {
        assert!(
            text.contains(&format!("?{mode}l")),
            "mode {mode} cleared: {text:?}"
        );
    }
}

/// A writer that refuses every write must surface the failure from the
/// leave path: the guard's release decides whether to swallow, but the
/// sequence builder reports rather than dropping the error silently.
#[test]
fn test_leave_propagates_write_failure() {
    let err = leave_sequences(&mut FailingWriter);
    assert!(
        err.is_err(),
        "a refused write propagates from the leave path"
    );
}

/// The progress indicator must be published on both transitions and cleared
/// on the way out.
#[test]
fn test_progress_sequence_both_ways() {
    let mut busy: Vec<u8> = Vec::new();
    write_progress(&mut busy, true).expect("busy written");
    assert!(
        String::from_utf8(busy).expect("ascii").contains("9;4;3"),
        "the busy form carries the indeterminate code"
    );

    let mut idle: Vec<u8> = Vec::new();
    write_progress(&mut idle, false).expect("cleared written");
    assert!(
        String::from_utf8(idle).expect("ascii").contains("9;4;0"),
        "the cleared form carries the stopped code"
    );
}

/// A terminal that refuses the progress write must not fail the teardown: the
/// clear is best effort, and the exit path continues to the mode restore.
#[test]
fn test_progress_clear_reports_failure() {
    clear_terminal_progress(&mut FailingWriter);
}

/// A setup failure where the undo also fails must still return the original
/// error rather than panicking on the second failure.
#[test]
fn test_setup_and_undo_fail() {
    let (m, enters, leaves) = modes(true, true);
    let err = TerminalGuard::acquire(m);
    assert!(err.is_err(), "the setup error is returned");
    assert_eq!(enters.get(), 1, "enter was attempted");
    assert_eq!(leaves.get(), 1, "the undo was attempted and swallowed");
}

/// The chrome that outlives the alternate screen is restored on the way out.
/// These write to the process stdout, which a test process captures; the
/// assertion is that neither step panics.
#[test]
fn test_host_chrome_restored() {
    restore_host_chrome();
    set_terminal_progress(true).expect("busy published");
    set_terminal_progress(false).expect("cleared");
}

/// A writer that refuses every write, so the best-effort failure branches run.
struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(Error::other("write refused"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(Error::other("flush refused"))
    }
}
