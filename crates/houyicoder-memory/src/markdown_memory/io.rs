//! Record parsing, serialization, key validation, and the atomic write
//! behind the markdown memory provider. A write lands through a writer-named
//! sibling temp file and a rename, so a reader never sees a half-written
//! record; a writer that dies in between leaves a temp the topic scan never
//! lists as a record.

use houyicoder_context::{
    MemoryEntry, MemoryError, MemoryOrigin, MemoryScope, MemorySummary, MemoryType, is_cjk,
};
use std::path::Path;

use super::MarkdownMemoryProvider;
#[cfg(test)]
use std::cell::Cell;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::read;
use std::fs::remove_file;
use std::fs::rename;
#[cfg(not(unix))]
use std::fs::write;
use std::io;
use std::io::ErrorKind;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::id;
use std::str::from_utf8;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use unicode_normalization::UnicodeNormalization;

/// Fields parsed from a topic file frontmatter.
#[derive(Debug, Clone)]
pub(super) struct Frontmatter {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) memory_type: MemoryType,
    pub(super) origin: MemoryOrigin,
}

/// One row of the advisory recall-stats sidecar. Serialized into the
/// per-scope .stats.json object keyed by memory key. gate_violations is
/// reserved; zero today, nothing feeds it yet.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct StatRecord {
    #[serde(default)]
    pub(super) recall_hits: u64,
    #[serde(default)]
    pub(super) gate_violations: u64,
    #[serde(default = "default_occurrences")]
    pub(super) occurrences: u64,
    #[serde(default)]
    pub(super) last_access_ts: u64,
}

fn default_occurrences() -> u64 {
    1
}

#[derive(Debug, Clone)]
pub(super) struct ScannedTopic {
    pub(super) key: String,
    pub(super) mtime: u64,
    pub(super) mtime_nanos: u128,
    pub(super) byte_len: u64,
    pub(super) path: PathBuf,
    pub(super) root_index: usize,
}

#[derive(Debug, Clone)]
pub(super) struct CachedTopic {
    pub(super) mtime_nanos: u128,
    pub(super) byte_len: u64,
    pub(super) frontmatter: Frontmatter,
    pub(super) lower_body: Arc<str>,
}

/// Parse a topic record into frontmatter plus a borrowed body slice without
/// copying the body on read-heavy ranking scans.
pub(super) fn parse_topic_slice(text: &str) -> Result<(Frontmatter, &str), MemoryError> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .or_else(|| {
            let first_end = text.find('\n')?;
            (text[..first_end].trim() == "---").then_some(&text[first_end + 1..])
        })
        .ok_or_else(|| MemoryError::Corrupt("frontmatter must open with ---".into()))?;

    let mut name = String::new();
    let mut description: &str = "";
    let mut memory_type = MemoryType::Project;
    let mut origin = MemoryOrigin::User;
    let mut offset = 0usize;
    let mut body_start = None;

    for raw_line in rest.split_inclusive('\n') {
        offset += raw_line.len();
        let line = raw_line.trim_end_matches(['\r', '\n']);
        if line.trim() == "---" {
            body_start = Some(offset);
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "name" => name = v.trim().to_string(),
            "description" => description = v.trim(),
            "type" | "source" => {
                memory_type = MemoryType::from_label(v)
                    .ok_or_else(|| MemoryError::Corrupt(format!("unknown type label: {v}")))?;
            }
            "origin" => origin = MemoryOrigin::from_label(v),
            _ => {}
        }
    }
    let start =
        body_start.ok_or_else(|| MemoryError::Corrupt("frontmatter never closed".into()))?;
    let body = rest[start..]
        .strip_suffix('\n')
        .map_or(&rest[start..], |s| s.strip_suffix('\r').unwrap_or(s));
    let raw_description = if description.is_empty() {
        first_line(body)
    } else {
        description
    };
    let description = cap_description(raw_description);
    Ok((
        Frontmatter {
            name,
            description,
            memory_type,
            origin,
        },
        body,
    ))
}

/// Parse a topic record into frontmatter plus an owned body string.
pub(super) fn parse_topic_file(text: &str) -> Result<(Frontmatter, String), MemoryError> {
    let (fm, body) = parse_topic_slice(text)?;
    Ok((fm, body.to_string()))
}

/// Resolve the stored writer provenance from raw topic bytes, defaulting an
/// unparseable or frontmatter-less file to User so background passes cannot
/// overwrite, archive, or delete hand-written notes.
pub(super) fn stored_origin_from_bytes(bytes: &[u8]) -> MemoryOrigin {
    match from_utf8(bytes)
        .ok()
        .and_then(|text| parse_topic_slice(text).ok())
    {
        Some((fm, _)) => fm.origin,
        None => MemoryOrigin::User,
    }
}

/// Read the stored writer provenance at path, returning None only when the
/// path is absent and surfacing I/O errors so an unreadable record fails
/// closed.
pub(super) fn stored_origin_at(path: &Path) -> Result<Option<MemoryOrigin>, MemoryError> {
    match read(path) {
        Ok(bytes) => Ok(Some(stored_origin_from_bytes(&bytes))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err(MemoryError::Io),
    }
}

/// Serialize a memory entry into a topic record (frontmatter plus body).
pub(super) fn serialize_topic_file(entry: &MemoryEntry) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("name: {}\n", entry.key));
    s.push_str(&format!("description: {}\n", index_description(entry)));
    s.push_str(&format!("type: {}\n", entry.memory_type.as_label()));
    s.push_str(&format!("origin: {}\n", entry.origin.as_label()));
    s.push_str("---\n");
    s.push_str(&entry.content);
    s
}

/// Security gate + normalizer for a memory key. NFC-normalizes the key so
/// decomposed and precomposed forms of the same grapheme map to one key
/// (prevents NFD/NFC dedup divergence in scan_candidates' HashMap), then
/// rejects path traversal, separators, drive letters, UNC prefixes, null
/// bytes, control characters, and reserved names so a crafted key cannot
/// escape the memory
/// root or poison the line-based MEMORY.md index. Returns the normalized
/// key so callers write, look up, and index under one canonical form.
pub(super) fn sanitize_key(key: &str) -> Result<String, MemoryError> {
    let key = key.nfc().collect::<String>();
    if key.is_empty() {
        return Err(MemoryError::InvalidPath("empty key".into()));
    }
    // The record and the temp file of its writer share one file name limit,
    // so a key near that limit would fail a write whose plain record name
    // still fit. The index line inherits the same bound.
    if key.len() > MAX_KEY_BYTES {
        return Err(MemoryError::InvalidPath("key too long".into()));
    }
    if key.contains('\0') {
        return Err(MemoryError::InvalidPath("null byte in key".into()));
    }
    // A control character in the key (newline, tab, 0x01-0x1F, 0x7F) would
    // break the line-based MEMORY.md index: the pointer line splits mid-entry
    // and the spillover reads as a second valid pointer (index poisoning).
    if key.chars().any(|c| c.is_control()) {
        return Err(MemoryError::InvalidPath("control character in key".into()));
    }
    if key.contains('/') || key.contains('\\') {
        return Err(MemoryError::InvalidPath("path separator in key".into()));
    }
    // Reject relative traversal fragments and leading dots.
    if key == ".." || key == "." || key.starts_with('.') {
        return Err(MemoryError::InvalidPath(
            "traversal or hidden prefix in key".into(),
        ));
    }
    // Reject Windows drive letters (case-insensitive C: forms).
    if key.len() >= 2 && key.as_bytes()[1] == b':' {
        return Err(MemoryError::InvalidPath("drive letter in key".into()));
    }
    // Reject UNC prefixes.
    if key.starts_with("\\\\") || key.starts_with("//") {
        return Err(MemoryError::InvalidPath("UNC prefix in key".into()));
    }
    if key.eq_ignore_ascii_case("MEMORY") {
        return Err(MemoryError::InvalidPath("reserved index name".into()));
    }
    Ok(key)
}

/// Reject a new key containing the sequence that separates a row's key from
/// its description. One index row cannot carry such a key unambiguously, so
/// a later read would attribute part of the key to the description.
/// A legacy key already on disk stays readable and deletable; writing one
/// again is refused like any other key that carries the sequence.
pub(super) fn validate_index_key(key: &str) -> Result<(), MemoryError> {
    if key.contains(": ") || key.contains(" [") {
        return Err(MemoryError::InvalidPath(
            "index row separator in key".into(),
        ));
    }
    Ok(())
}

/// Per-process counter separating the temp names one process writes inside
/// the same instant; the pid separates processes.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// Whether the next index write on this thread fails. A test uses it to
    /// hold a store in the state a writer that dies between a topic change
    /// and its index publish leaves behind.
    static FAIL_INDEX_WRITE: Cell<bool> = const { Cell::new(false) };
}

/// Make the next index write on this thread fail.
#[cfg(test)]
pub(super) fn fail_next_index_write() {
    FAIL_INDEX_WRITE.with(|flag| flag.set(true));
}

/// Write bytes to a path atomically. The bytes land in a sibling temp file
/// named for this writer, which is then renamed over the target, so a reader
/// never sees a half-written record. The temp name carries the process id and
/// a sequence number, so two processes writing one store never target the
/// same temp file, which a fixed name would let them truncate or rename out
/// from under each other. A failed write or rename removes the temp, so the
/// failure path leaves no residue behind.
pub(super) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    write_bytes(path, bytes, true)
}

/// Write bytes atomically without forcing them to disk. The rename already
/// makes the record appear whole, and an advisory counter can be rebuilt from
/// a cold start, so the durability flush is a cost with nothing to protect.
pub(super) fn write_bytes_advisory(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    write_bytes(path, bytes, false)
}

fn write_bytes(path: &Path, bytes: &[u8], durable: bool) -> Result<(), MemoryError> {
    // A test can fail the next index write to hold the state a writer leaves
    // when it dies between the topic change and the index publish.
    #[cfg(test)]
    if path.file_name().is_some_and(|name| name == INDEX_FILE)
        && FAIL_INDEX_WRITE.with(|flag| flag.replace(false))
    {
        return Err(MemoryError::Io);
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("memory");
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".{name}.{}.{seq}.tmp", id()));
    if write_temp(&temp, bytes, durable).is_err() {
        drop(remove_file(&temp));
        return Err(MemoryError::Io);
    }
    if rename(&temp, path).is_err() {
        drop(remove_file(&temp));
        return Err(MemoryError::Io);
    }
    Ok(())
}

/// Land a record at a new path, then remove the source. A rename that cannot
/// span two devices uses this. The destination is written through the atomic
/// path, so no reader lists it half-written, and the source stays until the
/// destination is durable.
pub(super) fn relocate_record(src: &Path, dst: &Path) -> Result<(), MemoryError> {
    let bytes = read(src).map_err(|_| MemoryError::Io)?;
    write_bytes_atomic(dst, &bytes)?;
    remove_file(src).map_err(|_| MemoryError::Io)
}

/// The platform write behind the atomic write. A durable write is flushed to
/// the device; an advisory one only has to land.
fn write_temp(path: &Path, bytes: &[u8], durable: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        if durable { f.sync_all() } else { f.flush() }
    }
    #[cfg(not(unix))]
    {
        let _ = durable;
        write(path, bytes)
    }
}

/// Truncate the index content to the line and byte caps, appending a warning
/// tail-note when a cap fires. Line-truncates first (natural boundary), then
/// byte-truncates at the last newline before the cap so the cut never lands
/// mid-line or mid-codepoint (multi-byte content like CJK would panic on a
/// raw String::truncate).
pub(super) fn cap_index_content(raw: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let was_line_truncated = lines.len() > max_lines;
    let was_byte_truncated = raw.len() > INDEX_BYTE_CAP;
    if !was_line_truncated && !was_byte_truncated {
        return raw.to_string();
    }
    // Line cap first (natural boundary).
    let mut truncated = if was_line_truncated {
        lines[..max_lines].join("\n")
    } else {
        raw.to_string()
    };
    // Byte cap at the last newline at or before the char boundary. The cut
    // must be on a UTF-8 char boundary to avoid String::truncate panic on
    // multi-byte content.
    if truncated.len() > INDEX_BYTE_CAP {
        let boundary = truncated
            .char_indices()
            .rev()
            .find(|(i, _)| *i <= INDEX_BYTE_CAP)
            .map(|(i, _)| i)
            .unwrap_or(INDEX_BYTE_CAP);
        let cut = truncated[..boundary]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(boundary);
        truncated.truncate(cut);
    }
    truncated.push('\n');
    truncated.push_str(INDEX_TRUNCATION_WARNING);
    truncated
}

/// First line of a block of text, trimmed; the fallback description for a
/// record written without a summary of its own.
pub(super) fn first_line(content: &str) -> &str {
    content.lines().next().unwrap_or("").trim()
}

/// Longest description a record publishes, in estimated tokens. The index
/// carries one row per entry into every request, so a long row spends the
/// shared budget on a single memory and pushes the rest out.
pub(super) const DESCRIPTION_TOKEN_CAP: usize = 24;

/// The one-line description a record publishes. The caller-supplied summary
/// wins; a record that carries none falls back to its first line so an entry
/// written before summaries existed still earns an index row. Control
/// characters are folded to spaces and the text is cut at the token cap,
/// because a newline or a long row would break the line-oriented index.
pub(super) fn index_description(entry: &MemoryEntry) -> String {
    let raw = if entry.description.trim().is_empty() {
        first_line(&entry.content).to_string()
    } else {
        entry.description.clone()
    };
    cap_description(&raw)
}

/// Fold to one line and cut at the description cap on a character boundary.
/// Cutting word by word would drop the rest of the budget as soon as one long
/// unspaced run (a CJK sentence, a path) crosses it, so the walk is per
/// character and retreats only when the cut would split a Latin word.
fn cap_description(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() <= DESCRIPTION_TOKEN_CAP * 3
        && !trimmed.contains(['\n', '\r', '\t'])
        && !trimmed.contains("  ")
    {
        return trimmed.to_string();
    }
    let mut compact = String::with_capacity(trimmed.len().min(128));
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_control() || c.is_whitespace() {
            if !compact.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                compact.push(' ');
                pending_space = false;
            }
            compact.push(c);
        }
    }
    let mut cjk = 0usize;
    let mut other = 0usize;
    let mut end_byte = compact.len();
    let mut last_char = None;
    let mut next_char = None;
    for (idx, c) in compact.char_indices() {
        let next_cjk = cjk + usize::from(is_cjk(c));
        let next_other = other + usize::from(!is_cjk(c));
        if next_cjk + next_other.div_ceil(4) > DESCRIPTION_TOKEN_CAP {
            end_byte = idx;
            next_char = Some(c);
            break;
        }
        cjk = next_cjk;
        other = next_other;
        last_char = Some(c);
    }
    if end_byte < compact.len() {
        if last_char.is_some_and(|c| c.is_ascii_alphanumeric())
            && next_char.is_some_and(|c| c.is_ascii_alphanumeric())
            && let Some(space) = compact[..end_byte].rfind(' ')
        {
            end_byte = space;
        }
        compact.truncate(end_byte);
        let trimmed_len = compact.trim_end().len();
        compact.truncate(trimmed_len);
    }
    compact
}

/// Byte cap on the derived MEMORY index (keeps the always-on index small).
pub(super) const INDEX_BYTE_CAP: usize = 25_000;

/// Longest key the store accepts, in bytes. A record name adds the markdown
/// suffix and a writing temp adds the process id and a sequence number, and
/// all of them must fit the platform's file name limit.
pub(super) const MAX_KEY_BYTES: usize = 200;

/// Reserved index filename (excluded from the topic scan).
pub(super) const INDEX_FILE: &str = "MEMORY.md";

/// Tail line a capped index carries. Its presence says the index omits
/// topics on purpose, so the omitted rows are not drift.
pub(super) const INDEX_TRUNCATION_WARNING: &str =
    "> WARNING: index too long, move detail to topic files.";

/// List every topic across all roots as a summary (key + description +
/// source + scope + mtime). The provider impl delegates here so the
/// enumeration logic lives in one place; the child module can reach the
/// parent's private scan_candidates + read_frontmatter.
pub(super) fn list_memories_impl(provider: &MarkdownMemoryProvider) -> Vec<MemorySummary> {
    provider
        .scan_candidates()
        .iter()
        .map(|t| {
            // Corrupt files (unparseable frontmatter) are included with a
            // sentinel description so the user sees them in the list rather
            // than having them silently excluded. The count reflects reality.
            match provider.read_frontmatter(&t.path) {
                Ok(fm) => MemorySummary::new(
                    t.key.clone(),
                    fm.description,
                    fm.memory_type,
                    MemoryScope::for_root_index(t.root_index),
                    t.mtime,
                )
                .with_origin(fm.origin),
                Err(_) => MemorySummary::new(
                    t.key.clone(),
                    "[corrupt topic file]",
                    MemoryType::Reference,
                    MemoryScope::for_root_index(t.root_index),
                    t.mtime,
                ),
            }
        })
        .collect()
}

/// Count topic files modified after the given timestamp (seconds since
/// epoch). The dream gate calls this to decide whether new material landed
/// since the last dream. Reuses scan_candidates so the count matches the
/// listing (deduped across roots, index file excluded).
pub(super) fn count_new_since_impl(provider: &MarkdownMemoryProvider, since: u64) -> usize {
    provider
        .scan_candidates()
        .iter()
        .filter(|t| t.mtime > since)
        .count()
}

/// Fetch one memory body from the named scope's root only, with no fallback to
/// another root — a fallback would hand back a different body than the one
/// asked for. The provider impl delegates here so the per-root read stays
/// beside the per-root enumeration.
pub(super) fn show_memory_in_scope_impl(
    provider: &MarkdownMemoryProvider,
    key: &str,
    scope: MemoryScope,
) -> Option<MemoryEntry> {
    let key = sanitize_key(key).ok()?;
    let topic = MarkdownMemoryProvider::scan_root(provider.root_for_scope(scope))
        .into_iter()
        .find(|t| t.key == key)?;
    provider.read_topic(&topic.path).ok()
}

/// The scopes holding a key, in root order. Reads every root instead of the
/// merged scan, which keeps only the newest copy per key and so cannot report
/// a second one.
pub(super) fn scopes_for_key_impl(
    provider: &MarkdownMemoryProvider,
    key: &str,
) -> Vec<MemoryScope> {
    let Ok(key) = sanitize_key(key) else {
        return Vec::new();
    };
    provider
        .roots
        .iter()
        .enumerate()
        .filter(|(_, root)| {
            MarkdownMemoryProvider::scan_root(root)
                .iter()
                .any(|t| t.key == key)
        })
        .map(|(index, _)| MemoryScope::for_root_index(index))
        .collect()
}

#[cfg(test)]
mod tests;
