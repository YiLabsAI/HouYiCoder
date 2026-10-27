//! What changes a worktree holds, split by class. The removal preview reports
//! the classes and the dirty-tree gate counts them, so both read one listing
//! and cannot disagree. The gate protects the classes that can hold work: it
//! counts an ignored entry no build name covers, and leaves known build output
//! out as regenerable.

use std::path::Path;

use super::session::WorktreeError;
use super::session::run_git_raw;

/// Directory names that hold build output. A preview reports a matching
/// ignored entry as build output rather than as unknown work, and the
/// dirty-work gate leaves it out, so a tree that differs from its baseline only
/// by build output counts as clean. The name decides the class, not what the
/// directory holds; an ignored entry this list does not name is counted as
/// unknown, so the user sees it.
const BUILD_DIRS: [&str; 9] = [
    "target",
    "node_modules",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    ".gradle",
];

/// The changes a worktree holds, by class. Clean trees report zero in every
/// field; a class a caller cannot read is never folded into another.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeChanges {
    /// Tracked files with staged or unstaged edits.
    pub tracked_dirty: usize,
    /// Paths Git does not track and an ignore rule does not cover.
    pub untracked: usize,
    /// Ignored directories a build-output name covers, as Git listed them,
    /// sorted and unique.
    pub ignored_build_dirs: Vec<String>,
    /// Ignored entries no build-output name covers. Counted, not named: a
    /// tree can hold any number of them, and the removal decision reads the
    /// count, so the record keeps no list that could grow without bound.
    pub ignored_unknown: usize,
}

/// The changes a worktree holds, with ignored entries split from the ones Git
/// tracks. The untracked detail is fixed rather than taken from the user's
/// configuration: listing every file of an untracked tree makes the child
/// reclaim walk directories it then discards, and it changes what one record
/// of the listing names.
///
/// Reading is not writing: the call takes no optional lock, so Git does not
/// refresh and rewrite the worktree's index behind the caller's back.
pub(super) fn read_changes(path: &Path) -> Result<WorktreeChanges, WorktreeError> {
    let raw = run_git_raw(
        path,
        &[
            // The optional lock a status run would take refreshes and can
            // rewrite the index, and this read is also used by the removal
            // preview, which promises to write nothing.
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--ignored=matching",
            "--untracked-files=normal",
            "-z",
        ],
    )?;
    Ok(classify_changes(&raw))
}

/// Split the NUL-separated records of a status listing into the classes a
/// removal must report. A rename names its source on the following record, so
/// that record is consumed rather than counted as a second entry.
fn classify_changes(raw: &[u8]) -> WorktreeChanges {
    let mut changes = WorktreeChanges::default();
    let mut build_dirs: Vec<String> = Vec::new();
    let mut fields = raw.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let status = &field[..2];
        let seen = String::from_utf8_lossy(&field[3..]).into_owned();
        match status {
            b"!!" => match build_entry(&seen) {
                Some(entry) => {
                    if !build_dirs.iter().any(|known| known == entry) {
                        build_dirs.push(entry.to_string());
                    }
                }
                None => changes.ignored_unknown += 1,
            },
            b"??" => changes.untracked += 1,
            _ => {
                changes.tracked_dirty += 1;
                if status.contains(&b'R') || status.contains(&b'C') {
                    let _ = fields.next();
                }
            }
        }
    }
    build_dirs.sort();
    changes.ignored_build_dirs = build_dirs;
    changes
}

/// The ignored directory when a build-output name covers it, without the
/// trailing slash Git puts on a directory. Only a directory carries build
/// output, so an ignored file that happens to carry a build name is left to the
/// unknown count rather than dropped from the gate. The name is matched against
/// the entry's own last segment, because Git reports a directory once however
/// deep it sits: a monorepo's node_modules under packages is the same
/// regenerable output as one at the repository root. The entry path is returned
/// whole, not just the name, so a preview can say where the output is.
fn build_entry(path: &str) -> Option<&str> {
    let trimmed = path.strip_suffix('/')?;
    let name = trimmed.rsplit('/').next().filter(|name| !name.is_empty())?;
    BUILD_DIRS.contains(&name).then_some(trimmed)
}

#[cfg(test)]
mod tests;
