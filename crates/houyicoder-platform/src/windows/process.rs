//! The native Windows launcher: the job exists before the command does, so
//! the process is a job member from its first instruction with no ungrouped
//! moment for a descendant to escape through. Output pipes stay here with
//! only their write ends crossing into the child.
//!
//! The pipes are anonymous handles, which have no overlapped form, so the
//! reads and the wait run on the runtime's blocking pool. A command therefore
//! parks one thread per pipe it has readers on, plus one while it is awaited,
//! for as long as the command or a descendant holding the write ends lives.

mod ffi;

use std::fs::File;
use std::io::{Error, Result};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::Arc;

use tokio::task::spawn_blocking;

/// A command started by the native launcher, with its output piped back.
///
/// The job handle, when there is one, is the lifetime of the whole command
/// tree: dropping it ends every process that entered the job. An unfenced
/// command is bounded only by its own process handle.
pub struct NativeChild {
    process: Arc<OwnedHandle>,
    job: Option<OwnedHandle>,
    stdout: Option<File>,
    stderr: Option<File>,
}

/// Start a command inside a newly created job. A command that cannot start is
/// reported before any process exists, so there is no partially grouped child
/// to clean up.
pub fn spawn_fenced(command_line: &str, cwd: &Path) -> Result<NativeChild> {
    let job = ffi::create_job()?;
    start_command(command_line, cwd, Some(job))
}

/// Start a command with no job, bounded only by its process handle. Used when
/// the session reports no kernel fence, so the launcher never implies
/// containment it is not providing.
pub fn spawn_unfenced(command_line: &str, cwd: &Path) -> Result<NativeChild> {
    start_command(command_line, cwd, None)
}

fn start_command(command_line: &str, cwd: &Path, job: Option<OwnedHandle>) -> Result<NativeChild> {
    let (stdout_read, stdout_write) = ffi::create_pipe()?;
    let (stderr_read, stderr_write) = ffi::create_pipe()?;
    let input = ffi::null_input()?;
    let stdio = [
        input.as_raw_handle(),
        stdout_write.as_raw_handle(),
        stderr_write.as_raw_handle(),
    ];
    let job_raw = job.as_ref().map(|job| job.as_raw_handle());
    let started = ffi::spawn(command_line, cwd, job_raw, stdio);
    // The child holds its own copies of the write ends. Keeping these open
    // would hide end of stream from the readers, which is the signal that
    // everything the command started has exited.
    drop(input);
    drop(stdout_write);
    drop(stderr_write);
    let process = started?;
    Ok(NativeChild {
        process: Arc::new(process),
        job,
        stdout: Some(File::from(stdout_read)),
        stderr: Some(File::from(stderr_read)),
    })
}

impl NativeChild {
    /// Take the read end of the command's standard output.
    pub fn take_stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }

    /// Take the read end of the command's standard error.
    pub fn take_stderr(&mut self) -> Option<File> {
        self.stderr.take()
    }

    /// Wait for the command to end and answer its exit status.
    ///
    /// The wait runs on the runtime's blocking pool: the system call parks
    /// until the process ends, and parking a worker thread would starve the
    /// runtime the caller runs on.
    pub async fn wait(&self) -> Result<ExitStatus> {
        let process = Arc::clone(&self.process);
        let code = spawn_blocking(move || ffi::wait(process.as_raw_handle()))
            .await
            .map_err(|error| Error::other(format!("the process wait did not finish: {error}")))??;
        Ok(ExitStatus::from_raw(code))
    }

    /// End the command now: the whole tree when it runs in a job, otherwise
    /// the direct process alone.
    pub fn end_tree(&mut self) {
        match self.job.take() {
            Some(job) => ffi::terminate_job(job.as_raw_handle()),
            None => ffi::terminate_process(self.process.as_raw_handle()),
        }
    }
}

impl Drop for NativeChild {
    fn drop(&mut self) {
        self.end_tree();
    }
}
