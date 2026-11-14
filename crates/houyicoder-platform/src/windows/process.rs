//! The native Windows launcher: the job exists before the command does, so
//! the process is a job member from its first instruction with no ungrouped
//! moment for a descendant to escape through. Output pipes stay here with
//! only their write ends crossing into the child.

mod ffi;

use std::io::{Error, Result};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::oneshot;

use self::ffi::ExitWait;
use super::pipe::{
    PipeDirection, SecurityDescriptor, create_instance, owner_only_descriptor, stream_endpoint,
};

/// Names the two pipes of one command apart from those of every other
/// command in this process. The process id separates concurrent processes.
static STREAM_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// A command started by the native launcher, with its output piped back.
///
/// The job handle, when there is one, is the lifetime of the whole command
/// tree: dropping it ends every process that entered the job. An unfenced
/// command is bounded only by its own process handle. Each output stream is
/// a named pipe instance read through the completion port, so an active
/// command parks no thread of this process.
pub struct NativeChild {
    process: OwnedHandle,
    job: Option<OwnedHandle>,
    stdout: Option<NamedPipeServer>,
    stderr: Option<NamedPipeServer>,
}

/// Start a command inside a newly created job. A command that cannot start is
/// reported before any process exists, so there is no partially grouped child
/// to clean up.
pub async fn spawn_fenced(command_line: &str, cwd: &Path) -> Result<NativeChild> {
    let job = ffi::create_job()?;
    start_command(command_line, cwd, Some(job)).await
}

/// Start a command with no job, bounded only by its process handle. Used when
/// the session reports no kernel fence, so the launcher never implies
/// containment it is not providing.
pub async fn spawn_unfenced(command_line: &str, cwd: &Path) -> Result<NativeChild> {
    start_command(command_line, cwd, None).await
}

async fn start_command(
    command_line: &str,
    cwd: &Path,
    job: Option<OwnedHandle>,
) -> Result<NativeChild> {
    let descriptor = owner_only_descriptor()?;
    let (stdout_reader, stdout_writer) = open_stream("out", &descriptor)?;
    let (stderr_reader, stderr_writer) = open_stream("err", &descriptor)?;
    let input = ffi::null_input()?;
    let stdio = [
        input.as_raw_handle(),
        stdout_writer.as_raw_handle(),
        stderr_writer.as_raw_handle(),
    ];
    let job_raw = job.as_ref().map(|job| job.as_raw_handle());
    let started = ffi::spawn(command_line, cwd, job_raw, stdio);
    // The child holds its own copies of the write ends. Keeping these open
    // would hide end of stream from the readers, which is the signal that
    // everything the command started has exited.
    drop(input);
    drop(stdout_writer);
    drop(stderr_writer);
    let process = started?;
    // The write end was opened before the command started, so each instance
    // is already connected and the handshake below completes without waiting.
    // A caller abandoned before it receives the child still ends the command
    // through the guard, which matters because an unfenced child has no job
    // whose close would stop it.
    let start = StartGuard::new(&process);
    // A stream that never connected has no reader, so the command would run
    // writing to nobody; the guard ends it on either return below.
    stdout_reader.connect().await?;
    stderr_reader.connect().await?;
    start.complete();
    Ok(NativeChild {
        process,
        job,
        stdout: Some(stdout_reader),
        stderr: Some(stderr_reader),
    })
}

/// Ends the command if the start is abandoned before the caller receives it,
/// which a dropped future does and an unfenced command has nothing else
/// bounding. A fenced command's job handle also closes on the same path.
struct StartGuard<'a> {
    process: Option<&'a OwnedHandle>,
}

impl<'a> StartGuard<'a> {
    fn new(process: &'a OwnedHandle) -> Self {
        Self {
            process: Some(process),
        }
    }

    /// The start finished, so dropping the guard leaves the command running.
    fn complete(mut self) {
        self.process = None;
    }
}

impl Drop for StartGuard<'_> {
    fn drop(&mut self) {
        if let Some(process) = self.process {
            ffi::terminate_process(process.as_raw_handle());
        }
    }
}

/// Create the instance this process reads one output stream from and the
/// write end its command inherits. The name carries the process and a
/// sequence so two commands never share a stream, and a fresh nonce so
/// another account cannot predict the next name and hold it with a first
/// instance of its own. The first-instance flag turns a name that somehow
/// already exists into a start failure rather than a channel shared with
/// another writer.
fn open_stream(
    label: &str,
    descriptor: &SecurityDescriptor,
) -> Result<(NamedPipeServer, OwnedHandle)> {
    let sequence = STREAM_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = stream_endpoint(label, sequence)?;
    let reader = create_instance(&name, descriptor, true, PipeDirection::Inbound)?;
    let writer = ffi::open_pipe_write_end(&name)?;
    Ok((reader, writer))
}

impl NativeChild {
    /// Take the read end of the command's standard output.
    pub fn take_stdout(&mut self) -> Option<NamedPipeServer> {
        self.stdout.take()
    }

    /// Take the read end of the command's standard error.
    pub fn take_stderr(&mut self) -> Option<NamedPipeServer> {
        self.stderr.take()
    }

    /// Wait for the command to end and answer its exit status.
    ///
    /// The wait is registered with the system's thread pool, which reports
    /// the exit code without a thread of this process parked on the command.
    pub async fn wait(&self) -> Result<ExitStatus> {
        let (report, exit) = oneshot::channel();
        let _wait = ExitWait::register(
            self.process.as_raw_handle(),
            Box::new(move |code| {
                let _sent = report.send(code);
            }),
        )?;
        let code = exit
            .await
            .map_err(|_| Error::other("the command exit report was lost"))?;
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
