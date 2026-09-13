//! Runtime directory capability state for a Seatbelt session.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use houyicoder_context::SandboxError;

#[derive(Clone, Default)]
pub(super) struct RuntimeDirs {
    read_only: Arc<Mutex<Vec<PathBuf>>>,
    read_write: Arc<Mutex<Vec<PathBuf>>>,
}

impl RuntimeDirs {
    pub(super) fn add_read(&self, path: &str) -> Result<(), SandboxError> {
        let canonical = canonical_dir(path)?;
        if self
            .read_write
            .lock()
            .expect("read-write dirs lock")
            .contains(&canonical)
        {
            return Ok(());
        }
        push_unique(&self.read_only, canonical);
        Ok(())
    }

    pub(super) fn add_write(&self, path: &str) -> Result<(), SandboxError> {
        let canonical = canonical_dir(path)?;
        remove_path(&self.read_only, &canonical);
        push_unique(&self.read_write, canonical);
        Ok(())
    }

    pub(super) fn remove(&self, path: &str) {
        remove_supplied(&self.read_only, path);
        remove_supplied(&self.read_write, path);
    }

    pub(super) fn remove_write(&self, path: &Path) {
        remove_path(&self.read_write, path);
    }

    pub(super) fn allows_read(&self, path: &Path) -> bool {
        contains_path(&self.read_only, path) || self.allows_write(path)
    }

    pub(super) fn allows_write(&self, path: &Path) -> bool {
        contains_path(&self.read_write, path)
    }

    pub(super) fn read_only(&self) -> Vec<PathBuf> {
        self.read_only.lock().expect("read-only dirs lock").clone()
    }

    pub(super) fn read_write(&self) -> Vec<PathBuf> {
        self.read_write
            .lock()
            .expect("read-write dirs lock")
            .clone()
    }

    pub(super) fn all_strings(&self) -> Vec<String> {
        self.read_only()
            .into_iter()
            .chain(self.read_write())
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }
}

fn canonical_dir(path: &str) -> Result<PathBuf, SandboxError> {
    let canonical = dunce::canonicalize(path)
        .map_err(|error| SandboxError::NotFound(format!("dir canonicalize: {path}: {error}")))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(SandboxError::NotFound(format!("not a directory: {path}")))
    }
}

fn push_unique(target: &Mutex<Vec<PathBuf>>, path: PathBuf) {
    let mut dirs = target.lock().expect("runtime dirs lock");
    if !dirs.contains(&path) {
        dirs.push(path);
    }
}

fn contains_path(target: &Mutex<Vec<PathBuf>>, path: &Path) -> bool {
    target
        .lock()
        .expect("runtime dirs lock")
        .iter()
        .any(|directory| path.starts_with(directory))
}

fn remove_path(target: &Mutex<Vec<PathBuf>>, path: &Path) {
    target
        .lock()
        .expect("runtime dirs lock")
        .retain(|directory| directory != path);
}

fn remove_supplied(target: &Mutex<Vec<PathBuf>>, path: &str) {
    if let Ok(canonical) = dunce::canonicalize(path) {
        remove_path(target, &canonical);
    } else {
        target
            .lock()
            .expect("runtime dirs lock")
            .retain(|directory| !directory.to_string_lossy().eq_ignore_ascii_case(path));
    }
}
