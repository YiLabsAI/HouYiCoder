//! Free-function helpers and supporting types for the markdown memory
//! provider. Split out of markdown_memory.rs so the provider module stays
//! under the file-size gate while the parsing, serialization, sanitization,
//! and atomic-write helpers live here. These are module-private (pub(super))
//! to the markdown_memory module — not part of any public surface.

use houyicoder_context::{
    MemoryEntry, MemoryError, MemoryOrigin, MemoryScope, MemorySource, MemorySummary,
};
use std::path::Path;

/// Fields parsed from a topic file frontmatter.
#[derive(Debug, Clone)]
pub(super) struct Frontmatter {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) source: MemorySource,
    pub(super) origin: MemoryOrigin,
}

/// One row of the advisory recall-stats sidecar. Serialized into the
/// per-scope .stats.json object keyed by memory key. gate_violations is
/// reserved (fed by the PreToolUse gate in a later sprint; zero until then).
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct StatRecord {
    #[serde(default)]
    pub(super) recall_hits: u64,
    #[serde(default)]
    pub(super) gate_violations: u64,
    #[serde(default)]
    pub(super) last_access_ts: u64,
}

/// A topic file located during the scan phase, with precomputed metadata
/// for ranking (no body read yet).
#[derive(Debug, Clone)]
pub(super) struct ScannedTopic {
    pub(super) key: String,
    pub(super) mtime: u64,
    pub(super) path: std::path::PathBuf,
    /// Position of the root this topic was scanned from, in the provider's
    /// ordered roots vec. Mapped to a MemoryScope at list time so the wire
    /// carries the physical scope (which root the surviving newest copy lives
    /// in, after the merge dedups a key that exists across scopes).
    pub(super) root_index: usize,
}

/// Parse a topic record into frontmatter plus body. The frontmatter is a
/// YAML-style block delimited by lines of three dashes.
pub(super) fn parse_topic_file(text: &str) -> Result<(Frontmatter, String), MemoryError> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() || lines[0].trim() != "---" {
        return Err(MemoryError::Corrupt(
            "frontmatter must open with ---".into(),
        ));
    }
    let mut name = String::new();
    let mut description = String::new();
    let mut source = MemorySource::Project;
    let mut origin = MemoryOrigin::Unknown;
    let mut close_line = None;
    for (i, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == "---" {
            close_line = Some(i);
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "name" => name = v.trim().to_string(),
            "description" => description = v.trim().to_string(),
            "source" => {
                source = MemorySource::from_label(v)
                    .ok_or_else(|| MemoryError::Corrupt(format!("unknown source label: {v}")))?;
            }
            "origin" => origin = MemoryOrigin::from_label(v),
            _ => {}
        }
    }
    let close =
        close_line.ok_or_else(|| MemoryError::Corrupt("frontmatter never closed".into()))?;
    let body = if close + 1 < lines.len() {
        lines[close + 1..].join("\n")
    } else {
        String::new()
    };
    Ok((
        Frontmatter {
            name,
            description,
            source,
            origin,
        },
        body,
    ))
}

/// Serialize a memory entry into a topic record (frontmatter plus body).
pub(super) fn serialize_topic_file(entry: &MemoryEntry) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("name: {}\n", entry.key));
    s.push_str(&format!("description: {}\n", first_line(&entry.content)));
    s.push_str(&format!("source: {}\n", entry.source.as_label()));
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
    use unicode_normalization::UnicodeNormalization;
    let key = key.nfc().collect::<String>();
    if key.is_empty() {
        return Err(MemoryError::InvalidPath("empty key".into()));
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

/// Write bytes to a path atomically: write to a sibling temp file, then
/// rename over the target. On Unix the rename is atomic.
pub(super) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("memory")
    ));
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|_| MemoryError::Io)?;
        f.write_all(bytes).map_err(|_| MemoryError::Io)?;
        f.sync_all().map_err(|_| MemoryError::Io)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&temp, bytes).map_err(|_| MemoryError::Io)?;
    }
    std::fs::rename(&temp, path).map_err(|_| MemoryError::Io)?;
    Ok(())
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
    truncated.push_str("\n> WARNING: index too long, move detail to topic files.");
    truncated
}

/// First line of a block of text, trimmed; used as the frontmatter
/// description derived from the entry content.
pub(super) fn first_line(content: &str) -> &str {
    content.lines().next().unwrap_or("").trim()
}

/// Byte cap on the derived MEMORY index (keeps the always-on index small).
pub(super) const INDEX_BYTE_CAP: usize = 25_000;

/// Reserved index filename (excluded from the topic scan).
pub(super) const INDEX_FILE: &str = "MEMORY.md";

/// Extract the rule sentence from a topic body: the first non-empty line
/// that is not a frontmatter fence or a Markdown heading marker. The rule
/// sentence is the always-on carrier line — the rule itself, not the
/// supporting Why / How-to-apply prose that follows.
pub(super) fn first_rule_sentence(content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "---" || trimmed.starts_with('#') {
            continue;
        }
        return trimmed.to_string();
    }
    String::new()
}

/// Merge a rule sentence into the project memory file (the always-on
/// carrier). Creates the file with a header + the rule line when missing.
/// Idempotent: when the rule sentence is already present, the file is
/// unchanged (no duplicate line). Returns an error only on a write fault
/// the caller logs and continues.
pub(super) fn merge_rule_into_carrier(path: &Path, rule: &str) -> Result<(), String> {
    if rule.is_empty() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == rule) {
        return Ok(());
    }
    let mut out = if existing.is_empty() {
        "# Project memory\n\n".to_string()
    } else {
        existing
    };
    // Append under a blank separator so the rule line stands alone (the
    // carrier reads as a list of one-line rules).
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(rule);
    out.push('\n');
    write_bytes_atomic(path, out.as_bytes()).map_err(|e| e.to_string())
}

/// Strip a rule sentence from the project memory file. Idempotent: when the
/// line is absent the file is unchanged. Returns an error only on a read or
/// write fault the caller logs and continues.
pub(super) fn strip_rule_from_carrier(path: &Path, rule: &str) -> Result<(), String> {
    if rule.is_empty() {
        return Ok(());
    }
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let filtered: Vec<&str> = existing
        .lines()
        .filter(|line| line.trim() != rule)
        .collect();
    let out = filtered.join("\n");
    if out == existing {
        return Ok(());
    }
    write_bytes_atomic(path, out.as_bytes()).map_err(|e| e.to_string())
}

/// List every topic across all roots as a summary (key + description +
/// source + scope + mtime). The provider impl delegates here so the
/// enumeration logic lives in one place; the child module can reach the
/// parent's private scan_candidates + read_frontmatter.
pub(super) fn list_memories_impl(provider: &super::MarkdownMemoryProvider) -> Vec<MemorySummary> {
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
                    fm.source,
                    MemoryScope::for_root_index(t.root_index),
                    t.mtime,
                )
                .with_origin(fm.origin),
                Err(_) => MemorySummary::new(
                    t.key.clone(),
                    "[corrupt topic file]",
                    MemorySource::Reference,
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
pub(super) fn count_new_since_impl(provider: &super::MarkdownMemoryProvider, since: u64) -> usize {
    provider
        .scan_candidates()
        .iter()
        .filter(|t| t.mtime > since)
        .count()
}
