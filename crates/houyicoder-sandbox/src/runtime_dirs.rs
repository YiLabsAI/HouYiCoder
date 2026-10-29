//! Runtime directory grants shared by the platform sessions: the dirs the
//! user authorized beyond the workspace root, canonicalized through dunce so
//! Windows and Unix compare in one form. Each backend re-derives its fence
//! from this state at the next spawn (macOS profile render, Linux fence
//! argv) or admits paths against it (Windows resolver).

use houyicoder_api::path::{expand_home, home_directory};
use houyicoder_context::SandboxError;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(crate) struct RuntimeDirs {
    read_only: Arc<Mutex<Vec<PathBuf>>>,
    read_write: Arc<Mutex<Vec<PathBuf>>>,
}

impl RuntimeDirs {
    /// Grant read access, reporting whether the grant set changed. A caller
    /// that owns a capability counter advances it on true, so a repeated
    /// grant does not look like a capability change.
    pub(crate) fn add_read(&self, path: &str) -> Result<bool, SandboxError> {
        let canonical = canonical_dir(path)?;
        if self
            .read_write
            .lock()
            .expect("read-write dirs lock")
            .contains(&canonical)
        {
            return Ok(false);
        }
        Ok(push_unique(&self.read_only, canonical))
    }

    pub(crate) fn add_write(&self, path: &str) -> Result<bool, SandboxError> {
        let canonical = canonical_dir(path)?;
        let was_read = remove_path(&self.read_only, &canonical);
        let added = push_unique(&self.read_write, canonical);
        Ok(added || was_read)
    }

    pub(crate) fn remove(&self, path: &str) -> bool {
        let was_read = remove_supplied(&self.read_only, path);
        let was_write = remove_supplied(&self.read_write, path);
        was_read || was_write
    }

    /// Drop one write grant by canonical path, leaving read grants alone.
    /// The worktree fence restore revokes the git-dir grant it added.
    #[cfg(target_os = "macos")]
    pub(crate) fn remove_write(&self, path: &Path) -> bool {
        remove_path(&self.read_write, path)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn remove_read(&self, path: &Path) -> bool {
        remove_path(&self.read_only, path)
    }

    pub(crate) fn allows_read(&self, path: &Path) -> bool {
        contains_path(&self.read_only, path) || self.allows_write(path)
    }

    pub(crate) fn allows_write(&self, path: &Path) -> bool {
        contains_path(&self.read_write, path)
    }

    pub(crate) fn read_only(&self) -> Vec<PathBuf> {
        self.read_only.lock().expect("read-only dirs lock").clone()
    }

    pub(crate) fn read_write(&self) -> Vec<PathBuf> {
        self.read_write
            .lock()
            .expect("read-write dirs lock")
            .clone()
    }

    pub(crate) fn all_strings(&self) -> Vec<String> {
        self.read_only()
            .into_iter()
            .chain(self.read_write())
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }
}

fn canonical_dir(path: &str) -> Result<PathBuf, SandboxError> {
    let canonical = dunce::canonicalize(resolve_supplied(path))
        .map_err(|error| SandboxError::NotFound(format!("dir canonicalize: {path}: {error}")))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(SandboxError::NotFound(format!("not a directory: {path}")))
    }
}

fn push_unique(target: &Mutex<Vec<PathBuf>>, path: PathBuf) -> bool {
    let mut dirs = target.lock().expect("runtime dirs lock");
    if dirs.contains(&path) {
        return false;
    }
    dirs.push(path);
    true
}

fn contains_path(target: &Mutex<Vec<PathBuf>>, path: &Path) -> bool {
    target
        .lock()
        .expect("runtime dirs lock")
        .iter()
        .any(|directory| path.starts_with(directory))
}

fn remove_path(target: &Mutex<Vec<PathBuf>>, path: &Path) -> bool {
    let mut dirs = target.lock().expect("runtime dirs lock");
    let before = dirs.len();
    dirs.retain(|directory| directory != path);
    dirs.len() != before
}

fn remove_supplied(target: &Mutex<Vec<PathBuf>>, path: &str) -> bool {
    let supplied = resolve_supplied(path);
    if let Ok(canonical) = dunce::canonicalize(&supplied) {
        return remove_path(target, &canonical);
    }
    // A path that no longer resolves is compared as written. Case is
    // compared loosely only where the file system does: elsewhere two
    // spellings that differ in case name two directories, and clearing the
    // wrong grant would revoke access the user did not ask to lose.
    let supplied_text = supplied.to_string_lossy();
    let mut dirs = target.lock().expect("runtime dirs lock");
    let before = dirs.len();
    dirs.retain(|directory| {
        let held = directory.to_string_lossy();
        if cfg!(windows) {
            !held.eq_ignore_ascii_case(&supplied_text)
        } else {
            held != supplied_text
        }
    });
    dirs.len() != before
}

/// The path the user meant by what they typed. A leading tilde is the shell's
/// spelling for the home directory, and the kernel resolves it as a literal
/// name, so the grant has to expand it before the file system is asked.
fn resolve_supplied(path: &str) -> PathBuf {
    expand_home(Path::new(path), home_directory().as_deref())
}
