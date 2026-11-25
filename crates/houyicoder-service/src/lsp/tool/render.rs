//! Rendering one tool answer: location and symbol rows, the call-hierarchy
//! rows, and the noise filter that keeps a workspace-wide search out of
//! build, vendor, and revision-control directories. The filter tests the
//! path behind a URI before a row is rendered, so a dropped location costs
//! no formatting work and its marker syntax can never leak into the answer.

use std::path::Path;
use std::path::PathBuf;
use std::str::from_utf8;
use std::sync::Arc;

use houyicoder_protocol::lsp::LspCallHierarchyItem;
use houyicoder_protocol::lsp::LspIncomingCall;
use houyicoder_protocol::lsp::LspLocation;
use houyicoder_protocol::lsp::LspLocationLink;
use houyicoder_protocol::lsp::LspOutgoingCall;
use houyicoder_protocol::lsp::LspRange;
use houyicoder_protocol::lsp::LspSymbolInformation;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::Value;

/// The directory names every answer drops, on top of the repository's own
/// ignore rules.
const NOISE_DIRS: &[&str] = &["target", "node_modules", ".git", "vendor"];

/// The repository's ignore rules plus the standard noise directories.
#[derive(Clone)]
pub(super) struct NoiseFilter {
    root: PathBuf,
    matcher: Option<Arc<Gitignore>>,
}

impl NoiseFilter {
    pub(super) fn new(root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        drop(builder.add(root.join(".gitignore")));
        let matcher = builder.build().ok().map(Arc::new);
        Self {
            root: root.to_path_buf(),
            matcher,
        }
    }

    /// Whether one file path points into a directory an answer should skip.
    pub(super) fn path_is_noise(&self, path: &Path) -> bool {
        for noise in NOISE_DIRS {
            if path.components().any(|part| part.as_os_str() == *noise) {
                return true;
            }
        }
        let Some(matcher) = &self.matcher else {
            return false;
        };
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        // The repository's rules only speak for paths inside it. A server
        // routinely answers with files outside the workspace, such as a
        // standard library or a dependency cache, and the matcher refuses
        // a path it does not own.
        if absolute.strip_prefix(&self.root).is_err() {
            return false;
        }
        matcher
            .matched_path_or_any_parents(&absolute, false)
            .is_ignore()
    }

    /// Whether the file behind one URI sits in a skipped directory.
    pub(super) fn uri_is_noise(&self, uri: &str) -> bool {
        self.path_is_noise(Path::new(&uri_to_path(uri)))
    }
}

/// The locations one definition or reference answer names, in every shape a
/// server may use: null, one location, an array, or location links. Rows in
/// skipped directories are dropped before rendering.
pub(super) fn locations_text(value: &Value, noise: &NoiseFilter) -> String {
    parse_locations(value)
        .into_iter()
        .filter(|(uri, _)| !noise.uri_is_noise(uri))
        .map(|(uri, range)| location_text(&uri, &range))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the location answer into (uri, range) pairs. The shape is read
/// from the keys a link carries before any decoding runs, so a large
/// reference list is decoded once in the shape it actually has rather than
/// probed through every variant.
fn parse_locations(value: &Value) -> Vec<(String, LspRange)> {
    match value {
        Value::Object(_) if value.get("targetUri").is_some() => {
            serde_json::from_value::<LspLocationLink>(value.clone())
                .map(|link| vec![(link.target_uri, link.target_range)])
                .unwrap_or_default()
        }
        Value::Object(_) => serde_json::from_value::<LspLocation>(value.clone())
            .map(|location| vec![(location.uri, location.range)])
            .unwrap_or_default(),
        Value::Array(items) => {
            let links = items
                .first()
                .and_then(Value::as_object)
                .is_some_and(|first| first.contains_key("targetUri"));
            if links {
                items
                    .iter()
                    .filter_map(|item| serde_json::from_value::<LspLocationLink>(item.clone()).ok())
                    .map(|link| (link.target_uri, link.target_range))
                    .collect()
            } else {
                items
                    .iter()
                    .filter_map(|item| serde_json::from_value::<LspLocation>(item.clone()).ok())
                    .map(|location| (location.uri, location.range))
                    .collect()
            }
        }
        _ => Vec::new(),
    }
}

/// One location as path:line:col, one-based, with the path readable.
pub(super) fn location_text(uri: &str, range: &LspRange) -> String {
    format!(
        "{}:{}:{}",
        uri_to_path(uri),
        range.start.line + 1,
        range.start.character + 1
    )
}

/// The file path behind one file URI, percent-decoded as bytes so a
/// multi-byte glyph survives and an escape naming a byte over 0x7f never
/// becomes a character of its own.
pub(super) fn uri_to_path(uri: &str) -> String {
    let mut trimmed = uri.strip_prefix("file://").unwrap_or(uri);
    // A Windows file URI carries the drive after the authority's slash,
    // which the path itself does not have; every other path keeps it.
    if trimmed.starts_with('/')
        && trimmed
            .chars()
            .nth(1)
            .zip(trimmed.chars().nth(2))
            .is_some_and(|(letter, colon)| letter.is_ascii_alphabetic() && colon == ':')
    {
        trimmed = &trimmed[1..];
    }
    let raw = trimmed.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        // An escape is kept as written unless it is a complete pair of
        // hex digits, so a stray percentage sign does not eat its neighbour.
        if raw[index] == b'%'
            && index + 2 < raw.len()
            && let Some(decoded) = from_utf8(&raw[index + 1..index + 3])
                .ok()
                .and_then(|text| u8::from_str_radix(text, 16).ok())
        {
            bytes.push(decoded);
            index += 3;
            continue;
        }
        bytes.push(raw[index]);
        index += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One flat symbol listing, the workspace search's shape. A symbol whose
/// file sits in a skipped directory never reaches the model.
pub(super) fn symbol_list_text(symbols: &[LspSymbolInformation], noise: &NoiseFilter) -> String {
    symbols
        .iter()
        .filter(|symbol| !noise.uri_is_noise(&symbol.location.uri))
        .map(|symbol| {
            format!(
                "{}: {}",
                symbol.name,
                location_text(&symbol.location.uri, &symbol.location.range)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The items a prepare answer names, one row per item, so the model sees
/// every overload the position offers before it asks for one.
pub(super) fn hierarchy_items_text(items: &[LspCallHierarchyItem], noise: &NoiseFilter) -> String {
    items
        .iter()
        .filter(|item| !noise.uri_is_noise(&item.uri))
        .map(|item| format!("{} ({})", item.name, location_text(&item.uri, &item.range)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One incoming call row, the caller naming where it calls from.
pub(super) fn incoming_calls_text(calls: &[LspIncomingCall], noise: &NoiseFilter) -> String {
    calls
        .iter()
        .filter(|call| !noise.uri_is_noise(&call.from.uri))
        .map(|call| {
            format!(
                "{} ← {}",
                call.from.name,
                location_text(&call.from.uri, &call.from.range)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One outgoing call row, the callee naming where it lives.
pub(super) fn outgoing_calls_text(calls: &[LspOutgoingCall], noise: &NoiseFilter) -> String {
    calls
        .iter()
        .filter(|call| !noise.uri_is_noise(&call.to.uri))
        .map(|call| {
            format!(
                "{} → {}",
                call.to.name,
                location_text(&call.to.uri, &call.to.range)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests;
