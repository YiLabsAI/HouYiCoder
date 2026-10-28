//! The files a reclaim pass reads to find live block addresses: session logs,
//! checkpoint manifests, and file write records. Which files count as sources
//! lives here, so the marking rules cover one list rather than three.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use houyicoder_context::session_class::{CHECKPOINT_DIR, FILE_TRANSACTION_DIR, LOG_FILE};

use super::{ReclaimError, unreadable};

/// One reference source: a file whose contents name live blocks.
pub(super) enum ReferenceSource {
    /// A session log, appended to one record at a time.
    Log { key: String, path: PathBuf },
    /// A checkpoint manifest, written whole.
    Checkpoint { key: String, path: PathBuf },
    /// A file write record, written whole and published atomically.
    Transaction { key: String, path: PathBuf },
}

impl ReferenceSource {
    pub(super) fn key(&self) -> &str {
        match self {
            Self::Log { key, .. }
            | Self::Checkpoint { key, .. }
            | Self::Transaction { key, .. } => key,
        }
    }

    pub(super) fn path(&self) -> &Path {
        match self {
            Self::Log { path, .. }
            | Self::Checkpoint { path, .. }
            | Self::Transaction { path, .. } => path,
        }
    }

    /// Whether this file only gains references. The session log is appended
    /// to, so a longer one still names every address the mark filed from it.
    /// A manifest or a write record is published whole, so the same length
    /// proves nothing about its contents.
    pub(super) fn is_append_only(&self) -> bool {
        matches!(self, Self::Log { .. })
    }
}

/// The sources a reference can be written to, in a stable order so a resumed
/// mark knows which ones it already filed.
///
/// Every step stops on an error other than a missing path. A session the pass
/// cannot name, stat, or open is a session whose references it does not hold,
/// and sweeping without them would free live blocks.
pub(super) fn list(root: &Path) -> Result<Vec<ReferenceSource>, ReclaimError> {
    let entries = fs::read_dir(root).map_err(|e| unreadable(root, e))?;
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| unreadable(root, e))?;
        let path = entry.path();
        // The store root also holds the reclaim workspace and the shards;
        // neither is a session, and neither is stat'ed on the way past.
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            return Err(unreadable(&path, "the session name is not UTF-8"));
        };
        if name.starts_with('.') {
            continue;
        }
        // Only a directory is a session; a stray file at the root names
        // no reference. A path that vanished is not a session either, and
        // any other stat error is a store this pass cannot read.
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(unreadable(&path, e)),
        }
        sessions.push((name, path));
    }
    sessions.sort();
    let mut sources = Vec::new();
    for (session, dir) in sessions {
        let log = dir.join(LOG_FILE);
        match fs::metadata(&log) {
            Ok(meta) if meta.is_file() => sources.push(ReferenceSource::Log {
                key: format!("log:{session}"),
                path: log,
            }),
            Ok(_) => return Err(unreadable(&log, "the log path is not a file")),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(unreadable(&log, e)),
        }
        sources.extend(records(&session, &dir.join(FILE_TRANSACTION_DIR))?);
        let checkpoints = dir.join(CHECKPOINT_DIR);
        let entries = match fs::read_dir(&checkpoints) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(unreadable(&checkpoints, e)),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| unreadable(&checkpoints, e))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => files.push(path),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(unreadable(&path, e)),
            }
        }
        files.sort();
        for path in files {
            let key = checkpoint_key(&session, &path)?;
            sources.push(ReferenceSource::Checkpoint { key, path });
        }
    }
    sources.sort_by(|a, b| a.key().cmp(b.key()));
    Ok(sources)
}

/// The write records of one session. A session that has recorded no write has
/// no directory, and one it cannot read stops the pass: an unread record
/// names blocks the sweep would otherwise free.
fn records(session: &str, dir: &Path) -> Result<Vec<ReferenceSource>, ReclaimError> {
    let operations = match fs::read_dir(dir) {
        Ok(operations) => operations,
        // A directory that does not exist yet is the ordinary case for a
        // session that has written no file. Any other failure stops the pass.
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(unreadable(dir, e)),
    };
    let mut files = Vec::new();
    for entry in operations {
        let entry = entry.map_err(|e| unreadable(dir, e))?;
        let operation = entry.path();
        // Each write owns a directory; a stray file names no record.
        match fs::metadata(&operation) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(unreadable(&operation, e)),
        }
        let entries = match fs::read_dir(&operation) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(unreadable(&operation, e)),
        };
        for entry in entries {
            let path = entry.map_err(|e| unreadable(&operation, e))?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => files.push(path),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(unreadable(&path, e)),
            }
        }
    }
    files.sort();
    let mut sources = Vec::new();
    for path in files {
        let key = transaction_key(session, &path)?;
        sources.push(ReferenceSource::Transaction { key, path });
    }
    Ok(sources)
}

/// The key one checkpoint manifest files under. A name that is not text
/// cannot key a source: two such manifests would collapse onto one key, and a
/// resumed mark would skip one of them while its references went unfiled.
pub(super) fn checkpoint_key(session: &str, path: &Path) -> Result<String, ReclaimError> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(unreadable(path, "the checkpoint name is not UTF-8"));
    };
    Ok(format!("cp:{session}/{name}"))
}

/// The key one write record files under: the session, the write it belongs
/// to, and which of that write's records it is. The write names itself with a
/// unique directory, so two records never share a key.
fn transaction_key(session: &str, path: &Path) -> Result<String, ReclaimError> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(unreadable(path, "the record name is not UTF-8"));
    };
    let operation = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    let Some(operation) = operation else {
        return Err(unreadable(path, "the write name is not UTF-8"));
    };
    Ok(format!("tx:{session}/{operation}/{name}"))
}
