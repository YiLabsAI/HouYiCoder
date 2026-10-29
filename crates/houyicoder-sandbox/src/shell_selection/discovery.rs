//! Gathering the Git Bash candidates on a Windows host, in trust order: the
//! settings path, the install locations the Git for Windows installer writes,
//! then whatever the system's own search names.
//!
//! Gathering only collects the paths and the directories they must stay out
//! of; the shared rules judge them, so a candidate from any tier is refused
//! unless it is an absolute path to bash whose real target sits outside them.

use super::rules::{Candidates, SearchOutcome, select, under_any, untrusted_roots};
use super::{ShellPreference, ShellProgram, ShellSelection};
use houyicoder_api::launcher::{
    FenceConfig, ProcessLauncher, SpawnPolicy, SpawnRequest, StdProcessLauncher,
};
use std::env::{current_dir, temp_dir, var_os};
use std::path::{Path, PathBuf};

/// The probe is bounded so a search that never returns cannot hold up startup.
const SEARCH_TIMEOUT_MS: u64 = 5_000;

pub(super) fn discover(
    preference: ShellPreference,
    configured: Option<&Path>,
    untrusted: &[PathBuf],
) -> ShellSelection {
    let working = current_dir().ok();
    let roots = untrusted_roots(untrusted, working.as_deref(), &temp_dir());
    if preference == ShellPreference::Cmd {
        // An explicit settings value runs the command interpreter without a
        // search, so nothing this host would have found can replace it.
        let candidates = Candidates {
            configured: None,
            standard: &[],
            searched: &[],
            search: SearchOutcome::Ran,
            untrusted: &roots,
        };
        return select(preference, &candidates, missing, no_real_path);
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
    // The install locations answer on a machine with an ordinary Git install,
    // so the system search, which starts a process, is asked only when they do
    // not.
    let first = settle(&[], SearchOutcome::Ran);
    if matches!(first.program, Some(ShellProgram::GitBash(_))) {
        return first;
    }
    // The search reads a path list that a working directory could influence,
    // so an unreadable working directory skips the tier rather than searching
    // without knowing what to keep out of it.
    if working.is_none() {
        tracing::warn!(
            "shell discovery: the working directory could not be read, so the system search is skipped"
        );
        return settle(&[], SearchOutcome::Unavailable);
    }
    let (searched, search) = search_path(&roots);
    settle(&searched, search)
}

/// Record the chosen shell where a developer can read it.
///
/// The user-facing line is the notice the caller displays; this one answers a
/// support question about which program ran a command, for a reader looking at
/// a log rather than the transcript.
fn report(selection: &ShellSelection) {
    if let Some(ShellProgram::GitBash(path)) = &selection.program {
        tracing::info!("shell: commands run with Git Bash at {}", path.display());
    }
}

fn is_file(path: &Path) -> bool {
    path.is_file()
}

fn missing(_: &Path) -> bool {
    false
}

fn real_path(path: &Path) -> Option<PathBuf> {
    dunce::canonicalize(path).ok()
}

fn no_real_path(_: &Path) -> Option<PathBuf> {
    None
}

/// The install locations a Git for Windows installer writes, the environment
/// variables first so a relocated install is found before the fixed paths.
fn standard_locations() -> Vec<PathBuf> {
    let mut out = Vec::new();
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

/// What the system's own search names for bash, in the order it lists them,
/// and whether the search ran at all.
///
/// The search tool itself is started by absolute path, from the system
/// directory, and the probe carries a wall timeout: a program found through
/// PATH would be the very hazard this search is trying to avoid, and an
/// unbounded probe would hold up the session that is waiting for an answer.
fn search_path(roots: &[PathBuf]) -> (Vec<PathBuf>, SearchOutcome) {
    let Some(search) = search_tool(roots) else {
        tracing::warn!(
            "shell discovery: the system search tool is missing or sits in a directory a candidate must stay out of"
        );
        return (Vec::new(), SearchOutcome::Unavailable);
    };
    let request = SpawnRequest::new(search.to_string_lossy().into_owned())
        .with_args(["bash.exe"])
        .piped_output();
    let policy = SpawnPolicy::default()
        .with_fence(FenceConfig {
            wall_timeout_ms: SEARCH_TIMEOUT_MS,
            ..FenceConfig::default()
        })
        .audited();
    let launcher = StdProcessLauncher::new();
    let Ok(child) = launcher.spawn(request, policy) else {
        tracing::warn!("shell discovery: the system search for bash could not be started");
        return (Vec::new(), SearchOutcome::Unavailable);
    };
    let exit = match pollster::block_on(child.wait()) {
        Ok(exit) => exit,
        Err(e) => {
            tracing::warn!("shell discovery: the system search for bash failed: {e}");
            return (Vec::new(), SearchOutcome::Unavailable);
        }
    };
    // The search tool answers with a failing status when nothing matches,
    // which is a result rather than an error.
    if !matches!(exit.exit_code, Some(0) | Some(1)) {
        tracing::warn!(
            "shell discovery: the system search exited with status {:?}",
            exit.exit_code
        );
        return (Vec::new(), SearchOutcome::Unavailable);
    }
    let found = exit
        .stdout
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect();
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
