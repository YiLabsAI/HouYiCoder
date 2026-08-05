//! The diagnostic sink: a file-backed tracing subscriber with a filter that
//! can be changed while the process runs.
//!
//! Three sinks carry a message, and the choice between them is a design
//! decision. A message the user must know about, or can act on, belongs in
//! the transcript as a system line. A message only a developer can use
//! belongs here. A print macro is correct only where no alternate screen is
//! up, such as argument parsing.
//!
//! The destination is a file and nothing else. The TUI runs in the
//! terminal's alternate screen, which does not capture stdout or stderr, so
//! the terminal paints a write at wherever the cursor sits -- during a
//! session that is inside the input box. install takes a path rather than a
//! writer so a caller cannot hand this module the terminal; the restriction
//! is carried by the signature, not by a comment asking the next reader to
//! be careful.
//!
//! One subscriber serves every crate that uses the tracing macros. That is
//! the point of installing it here rather than per crate: a filter change
//! reaches the engine, the sandbox and the permission gate at once. Two
//! hand-rolled loggers preceded this one, each with its own switch, and a
//! switch flipped at runtime reached only one of them -- so the log looked
//! healthy while the half a reader most needed was silently absent.

use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Registry, fmt, reload};

/// The handle stored at install time, if install was called in this process.
///
/// The subscriber itself is process-wide (tracing's default), so its control
/// surface is process-wide too. Storing the handle here is not a new hidden
/// global: it is the control for a global that already exists. A caller that
/// did not install a sink (the loader binary, a test) reads None, and a
/// /debug wire request against a server with no sink returns an error rather
/// than silently succeeding.
static HANDLE: OnceLock<Option<DiagnosticsHandle>> = OnceLock::new();

/// Why installing the diagnostic sink failed. Creating the file is the only
/// fallible step a caller can act on; a second install is a wiring mistake
/// rather than a runtime condition, but it is reported rather than ignored
/// so a composition root cannot silently end up with two sinks.
#[derive(Debug)]
pub enum InstallError {
    /// The log file could not be created at the requested path.
    File(std::io::Error),
    /// A global subscriber was already installed in this process.
    AlreadyInstalled,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(e) => write!(f, "could not open the diagnostic log: {e}"),
            Self::AlreadyInstalled => {
                write!(f, "a diagnostic subscriber is already installed")
            }
        }
    }
}

impl std::error::Error for InstallError {}

/// The runtime control surface for the diagnostic sink.
///
/// Deliberately narrow: a level and an off switch. It exposes no way to
/// redirect the sink, because the only safe destination was fixed when the
/// sink was installed.
#[derive(Clone)]
pub struct DiagnosticsHandle {
    inner: reload::Handle<Targets, Registry>,
    path: std::path::PathBuf,
}

impl DiagnosticsHandle {
    /// The file the sink writes to. Fixed at install time; a /debug reply
    /// carries it so the user is told where to look.
    pub fn path(&self) -> Option<std::path::PathBuf> {
        Some(self.path.clone())
    }

    /// Raise or lower the level every crate logs at, effective immediately
    /// and without a restart. This is what a debug command drives.
    ///
    /// The error case is a dropped subscriber, which cannot happen while the
    /// process holds the global default; it is returned rather than ignored
    /// so a future caller that stores a handle past teardown learns about it.
    pub fn set_level(&self, level: LevelFilter) -> Result<(), reload::Error> {
        self.inner
            .modify(|filter| *filter = Targets::new().with_default(level))
    }

    /// Stop recording. Events are discarded at the macro call site, so a
    /// disabled sink costs the caller a filter check and no formatting.
    pub fn disable(&self) -> Result<(), reload::Error> {
        self.set_level(LevelFilter::OFF)
    }
}

fn open_log_file(path: &Path) -> Result<std::fs::File, InstallError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(InstallError::File)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(InstallError::File)
}

/// Compose the subscriber and its control handle, without claiming the
/// process-wide subscriber slot.
///
/// Separate from install because claiming that slot is a one-time global act:
/// a test that performs it fixes the behaviour of every other test in the
/// same binary and can never undo it. Composition holds the decisions worth
/// asserting -- which filters, in what order, to which writer -- so it stays
/// reachable on its own and a test drives it as a scoped default instead.
///
/// Starts disabled, so a normal run pays a filter check per call site and no
/// formatting. ANSI colouring is off because the destination is a file read
/// with a pager or a grep, where escape sequences are noise.
fn build(
    file: std::fs::File,
    path: std::path::PathBuf,
) -> (
    impl tracing::Subscriber + Send + Sync + 'static,
    DiagnosticsHandle,
) {
    let (filter, handle) = reload::Layer::new(Targets::new().with_default(LevelFilter::OFF));
    // The filter is layered directly onto the registry, ahead of the format
    // layer. Both orders filter identically, because a Targets added as a
    // layer is a global filter rather than a per-layer one, but this order
    // keeps the reload handle parameterised by Registry alone. With the
    // format layer first the handle's type carries that layer too, which
    // would put a writer type in the signature of every function passing
    // the handle around.
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(Mutex::new(file)).with_ansi(false));
    (
        subscriber,
        DiagnosticsHandle {
            inner: handle,
            path,
        },
    )
}

/// Install the process-wide diagnostic sink, writing to path, and return the
/// handle that changes its level later.
pub fn install(path: &Path) -> Result<DiagnosticsHandle, InstallError> {
    let (subscriber, handle) = build(open_log_file(path)?, path.to_path_buf());
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| InstallError::AlreadyInstalled)?;
    // Store the handle for callers that did not receive it directly (the
    // composition root's pair_inproc_server reads it via handle()). set
    // returns Err if already set; a second install is already an error
    // above, so the Err here is impossible in practice and dropped.
    if HANDLE.set(Some(handle.clone())).is_err() {
        // Already set: a prior install won. Keep the first one.
    }
    Ok(handle)
}

/// The handle stored at install time, if install was called in this process.
/// None when no sink was installed (the loader binary, a test). A server
/// constructed without a handle rejects the /debug wire command rather than
/// silently accepting it.
pub fn handle() -> Option<DiagnosticsHandle> {
    HANDLE.get().and_then(|h| h.as_ref().cloned())
}

/// The path the sink was installed at, if any. The /debug reply carries this
/// so the user is told where the file is rather than guessing. Read from the
/// stored handle's current filter state — the path is fixed at install time,
/// so this is stable, but reading it through the handle keeps the source of
/// truth in one place.
pub fn installed_path() -> Option<std::path::PathBuf> {
    HANDLE.get().and_then(|h| h.as_ref()).and_then(|h| h.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("houyi-diag-{tag}-{}", std::process::id()));
        drop(std::fs::remove_dir_all(&d));
        d
    }

    /// A path whose parent does not exist yet still opens: naming a log under
    /// a fresh directory should not require creating it first.
    #[test]
    fn test_open_creates_parent_dirs() {
        let dir = temp_dir("parent");
        let path = dir.join("nested").join("debug.log");
        drop(open_log_file(&path).expect("open under a fresh parent"));
        assert!(path.exists(), "the log file was not created");
        drop(std::fs::remove_dir_all(&dir));
    }

    /// A parent that is a regular file cannot hold a log, and the caller is
    /// told at once rather than discovering later that nothing was recorded.
    #[test]
    fn test_open_reports_bad_parent() {
        let dir = temp_dir("badparent");
        std::fs::create_dir_all(&dir).expect("create the test dir");
        let blocker = dir.join("a-file");
        std::fs::write(&blocker, b"not a directory").expect("write the blocker");
        let err = open_log_file(&blocker.join("debug.log")).expect_err("must not open");
        assert!(
            matches!(err, InstallError::File(_)),
            "expected a file error, got {err:?}"
        );
        assert!(
            err.to_string().contains("diagnostic log"),
            "the message should name what failed, got {err}"
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    /// The already-installed case is a wiring mistake, so its message names
    /// the slot that was taken instead of quoting an underlying error.
    #[test]
    fn test_already_installed_names_slot() {
        assert!(
            InstallError::AlreadyInstalled
                .to_string()
                .contains("already installed"),
            "unexpected message: {}",
            InstallError::AlreadyInstalled
        );
    }

    /// The handle turns recording on and off, and the file is the witness.
    /// Asserting the handle returned Ok would prove only that a filter was
    /// swapped, not that the swap changed what gets written -- and what gets
    /// written is the whole point.
    ///
    /// Driven as a scoped default so this test does not fix the subscriber
    /// for the rest of the binary.
    #[test]
    fn test_handle_toggles_recording() {
        let dir = temp_dir("toggle");
        let path = dir.join("debug.log");
        let (subscriber, handle) = build(open_log_file(&path).expect("open"), path.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("while disabled");
            handle.set_level(LevelFilter::DEBUG).expect("raise");
            tracing::debug!("while enabled");
            handle.disable().expect("disable");
            tracing::debug!("after disabling");
        });
        let body = std::fs::read_to_string(&path).expect("read the log");
        assert!(
            !body.contains("while disabled"),
            "a disabled sink recorded anyway: {body}"
        );
        assert!(
            body.contains("while enabled"),
            "an enabled sink recorded nothing: {body}"
        );
        assert!(
            !body.contains("after disabling"),
            "the switch latched on instead of turning off: {body}"
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    /// The handle carries the path it was installed with, so a /debug reply
    /// can show the user where the file is without guessing. Drives build
    /// directly rather than install, so it does not claim the process-wide
    /// subscriber slot.
    #[test]
    fn test_handle_carries_path() {
        let dir = temp_dir("path");
        let path = dir.join("debug.log");
        let (_subscriber, handle) = build(open_log_file(&path).expect("open"), path.clone());
        let got = handle.path().expect("the handle carries a path");
        assert_eq!(got, path, "the path is the one the handle was built with");
        drop(std::fs::remove_dir_all(&dir));
    }

    /// install claims the process-wide subscriber slot, stores the handle for
    /// handle(), and returns it. A second call fails with AlreadyInstalled
    /// because the slot is one-time. This test claims the slot for the rest
    /// of the lib test process; the with_default-scoped tests above are
    /// unaffected because with_default does not touch the global default.
    #[test]
    fn test_install_claims_rejects_second() {
        let dir = temp_dir("install");
        let path = dir.join("debug.log");
        let h1 = install(&path).expect("first install succeeds");
        // handle() now sees the stored handle.
        assert!(handle().is_some(), "handle() returns Some after install");
        // installed_path() reads the path from the stored handle.
        let got = installed_path().expect("installed_path returns Some");
        assert_eq!(got, path, "the path matches what was installed");
        // A second install fails: the global slot is already taken.
        if let Err(e) = install(&path) {
            assert!(
                matches!(e, InstallError::AlreadyInstalled),
                "expected AlreadyInstalled, got {e}"
            );
        } else {
            panic!("second install must fail with AlreadyInstalled");
        }
        // The first handle still works.
        assert!(h1.path().is_some(), "the first handle is still valid");
        drop(std::fs::remove_dir_all(&dir));
    }
}
