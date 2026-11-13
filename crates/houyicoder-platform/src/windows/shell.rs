//! Windows shell discovery: gathering the Git Bash candidates in trust order,
//! and resolving the command interpreter and PowerShell by absolute path.
//!
//! The search subprocess is started with the no-window flag and a wall timeout,
//! so a probe that never returns cannot hold up startup or flash a console.

use std::env::{current_dir, temp_dir, var_os};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use std::os::windows::process::CommandExt;

use crate::host::shell::{
    Candidates, HelperShell, PwshEdition, SearchOutcome, ShellPreference, ShellProgram,
    ShellSelection, bash_from_git, locations_from_env, select, under_any, untrusted_roots,
};

/// The no-window flag passed to every probe the search starts.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The probe is bounded so a search that never returns cannot hold up startup.
const SEARCH_TIMEOUT_MS: u64 = 5_000;

pub(crate) fn discover(
    preference: ShellPreference,
    configured: Option<&Path>,
    untrusted: &[PathBuf],
) -> ShellSelection {
    let working = current_dir().ok();
    let roots = untrusted_roots(untrusted, working.as_deref(), &temp_dir());
    if preference == ShellPreference::Cmd {
        let candidates = Candidates {
            configured: None,
            standard: &[],
            searched: &[],
            search: SearchOutcome::Ran,
            untrusted: &roots,
        };
        return select(preference, &candidates, missing, no_real_path);
    }
    if let ShellPreference::PowerShell(wanted) = preference {
        return resolve_pwsh()
            .and_then(|(path, edition)| match wanted {
                Some(requested) if requested != edition => None,
                _ => Some((path, edition)),
            })
            .map(|(path, edition)| {
                let program = ShellProgram::PowerShell { path, edition };
                tracing::info!("shell: commands run with PowerShell");
                ShellSelection {
                    program: Some(program),
                    notice: None,
                }
            })
            .unwrap_or_else(|| {
                let not_found = match wanted {
                    Some(PwshEdition::Core) => "PowerShell 7 was asked for but pwsh was not found",
                    Some(PwshEdition::Desktop) => {
                        "Windows PowerShell 5.1 was asked for but powershell was not found"
                    }
                    None => "PowerShell was asked for but neither pwsh nor powershell was found",
                };
                ShellSelection {
                    program: None,
                    notice: Some(format!(
                        "{not_found}, so commands run with the Windows command interpreter"
                    )),
                }
            });
    }
    let standard = standard_locations();
    let settle = |searched: &[PathBuf], search: SearchOutcome| {
        let candidates = Candidates {
            configured,
            standard: &standard,
            searched,
            search,
            untrusted: &roots,
        };
        let selection = select(preference, &candidates, is_file, real_path);
        report(&selection);
        selection
    };
    let first = settle(&[], SearchOutcome::Ran);
    if matches!(first.program, Some(ShellProgram::GitBash(_))) {
        return first;
    }
    if working.is_none() {
        return settle(&[], SearchOutcome::Unavailable);
    }
    let (searched, search) = search_path(&roots);
    settle(&searched, search)
}

fn missing(_: &Path) -> bool {
    false
}

fn no_real_path(_: &Path) -> Option<PathBuf> {
    None
}

fn report(selection: &ShellSelection) {
    if let Some(ShellProgram::GitBash(path)) = &selection.program {
        tracing::info!("shell: commands run with Git Bash at {}", path.display());
    }
}

fn is_file(path: &Path) -> bool {
    path.is_file()
}

fn real_path(path: &Path) -> Option<PathBuf> {
    dunce::canonicalize(path).ok()
}

/// The system's own command interpreter, by absolute path where the host
/// supplies one.
pub(crate) fn command_interpreter() -> PathBuf {
    if let Some(root) = var_os("SystemRoot") {
        return PathBuf::from(root).join("System32").join("cmd.exe");
    }
    if let Some(shell) = var_os("COMSPEC") {
        return PathBuf::from(shell);
    }
    PathBuf::from("cmd")
}

/// The install locations a Git for Windows installer writes, the environment
/// variables first so a relocated install is found before the fixed paths.
///
/// A user can name the bash directly, name the install root, or install to a
/// disk the fixed paths never cover with only the cmd directory on PATH. The
/// variables answer the first two; the fixed paths cover the standard install.
fn standard_locations() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let direct = var_os("HOUYICODER_GIT_BASH_PATH")
        .or_else(|| var_os("CLAUDE_CODE_GIT_BASH_PATH"))
        .map(PathBuf::from);
    let roots = var_os("GIT_INSTALL_ROOT").map(PathBuf::from);
    out.extend(locations_from_env(direct.as_deref(), roots.as_deref()));
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = var_os(variable) {
            out.push(PathBuf::from(base).join("Git").join("bin").join("bash.exe"));
        }
    }
    if let Some(base) = var_os("LOCALAPPDATA") {
        out.push(
            PathBuf::from(base)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    out.push(PathBuf::from("C:\\Program Files\\Git\\bin\\bash.exe"));
    out.push(PathBuf::from("C:\\Program Files (x86)\\Git\\bin\\bash.exe"));
    out
}

/// What the system's own search names for bash, and whether the search ran.
///
/// The search asks for bash and for the git program: an install on a second
/// disk often puts only its cmd directory on PATH, so the search for bash
/// misses it while the search for git names the install, and the parents of
/// the git path hold the bash the install shipped.
fn search_path(roots: &[PathBuf]) -> (Vec<PathBuf>, SearchOutcome) {
    let Some(search) = search_tool(roots) else {
        return (Vec::new(), SearchOutcome::Unavailable);
    };
    let Some(bash) = run_bounded(&search, &["bash.exe"]) else {
        return (Vec::new(), SearchOutcome::Unavailable);
    };
    if !matches!(bash.status, Some(0) | Some(1)) {
        return (Vec::new(), SearchOutcome::Unavailable);
    }
    let mut found: Vec<PathBuf> = bash
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect();
    // The git program search is best effort: when it cannot run the bash
    // entries already collected stand, so the outcome stays Ran.
    if let Some(git) = run_bounded(&search, &["git.exe"]) {
        if matches!(git.status, Some(0) | Some(1)) {
            for path in git
                .stdout
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
            {
                let git_path = PathBuf::from(path);
                if !git_path.is_absolute() {
                    continue;
                }
                for candidate in bash_from_git(&git_path, is_file) {
                    if !found.contains(&candidate) {
                        found.push(candidate);
                    }
                }
            }
        }
    }
    (found, SearchOutcome::Ran)
}

/// The absolute path of the system search tool, refused when it sits where the
/// working tree could have put it.
fn search_tool(roots: &[PathBuf]) -> Option<PathBuf> {
    let candidate = var_os("SystemRoot").map_or_else(
        || PathBuf::from("C:\\Windows\\System32\\where.exe"),
        |root| PathBuf::from(root).join("System32").join("where.exe"),
    );
    (candidate.is_file() && !under_any(&candidate, roots)).then_some(candidate)
}

/// The captured output of a bounded probe.
struct ProbeOutput {
    status: Option<i32>,
    stdout: String,
}

/// Run a probe and read its output within a bounded window. This sits below the
/// launcher port: it is a platform primitive with no runtime dependency.
#[expect(
    clippy::disallowed_methods,
    reason = "L0 platform probe below the ProcessLauncher port; no runtime dependency"
)]
fn run_bounded(program: &Path, args: &[&str]) -> Option<ProbeOutput> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd.spawn().ok()?;
    let drain = child.stdout.take().map(|mut reader| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            drop(reader.read_to_end(&mut buf));
            buf
        })
    });
    let status = wait_bounded(&mut child, Duration::from_millis(SEARCH_TIMEOUT_MS));
    drop(child.kill());
    drop(child.wait());
    let bytes = drain
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    Some(ProbeOutput {
        status,
        stdout: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// Poll the child until it exits or the deadline passes.
fn wait_bounded(child: &mut Child, limit: Duration) -> Option<i32> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// Resolve PowerShell by absolute path: the core build first, then the Windows
/// desktop one. Returns the path and the edition it named.
pub(crate) fn resolve_pwsh() -> Option<(PathBuf, PwshEdition)> {
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = var_os(variable) {
            let path = PathBuf::from(base)
                .join("PowerShell")
                .join("7")
                .join("pwsh.exe");
            if path.is_file() {
                return Some((path, PwshEdition::Core));
            }
        }
    }
    if let Some(root) = var_os("SystemRoot") {
        let path = PathBuf::from(root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        if path.is_file() {
            return Some((path, PwshEdition::Desktop));
        }
    }
    None
}

/// Resolve a helper shell to the program that runs it. The sh and cmd variants
/// name a program the host resolves; Git Bash and PowerShell carry an absolute
/// path this returns None for when the program is absent.
pub(crate) fn helper_program(shell: HelperShell) -> Option<ShellProgram> {
    match shell {
        HelperShell::Sh => Some(ShellProgram::Posix),
        HelperShell::GitBash => {
            let selection = discover(ShellPreference::GitBash, None, &[]);
            selection.program.and_then(|program| match program {
                ShellProgram::GitBash(path) => Some(ShellProgram::GitBash(path)),
                _ => None,
            })
        }
        HelperShell::Cmd => Some(ShellProgram::Cmd),
        HelperShell::PowerShell => {
            resolve_pwsh().map(|(path, edition)| ShellProgram::PowerShell { path, edition })
        }
    }
}

#[cfg(test)]
mod tests;
