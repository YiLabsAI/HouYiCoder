//! Rule matching across a leading move into the session root. A command that
//! begins by moving into the root acts on the files a rule recorded there
//! names, so the rule set is consulted a second time without that move. The
//! destination is resolved on disk: a command that moves anywhere else is a
//! different authorization and does not inherit a rule made for the root.

use houyicoder_api::sandbox::normalize_tool_path;
use std::path::{Path, PathBuf};

use crate::compound::split_compound;

/// The command with its leading move into the session root removed, or None
/// when the command does not begin with one. Only a move joined by and-then
/// counts: a semicolon runs the rest of the command even when the move
/// fails, so that rest would run in a directory the rule never named.
pub(crate) fn root_relative_content(command: &str, root: &Path) -> Option<String> {
    let segments = split_compound(command);
    if segments.len() < 2 {
        return None;
    }
    let target = literal_move_target(&segments[0])?;
    if !resolves_to(target, root) {
        return None;
    }
    let rest = command.strip_prefix(segments[0].as_str())?.trim_start();
    let rest = rest.strip_prefix("&&")?.trim_start();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// The destination of a move that names one plain directory. An expansion or
/// a redirect would make the destination something this check cannot see, so
/// anything but two plain words is refused.
fn literal_move_target(segment: &str) -> Option<&str> {
    let mut tokens = segment.split_whitespace();
    if tokens.next()? != "cd" {
        return None;
    }
    let dir = tokens.next()?;
    if tokens.next().is_some() || dir.contains(['$', '`', '>', '<']) {
        return None;
    }
    Some(dir)
}

/// Whether the destination resolves to the root itself. A relative spelling
/// is joined to the root, which is where a shell tool starts. Both sides go
/// through the same resolver, so a symlink or a dot-dot spelling is compared
/// by where it lands rather than by how it is written.
fn resolves_to(target: &str, root: &Path) -> bool {
    let candidate = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        root.join(target)
    };
    match (normalize_tool_path(&candidate), normalize_tool_path(root)) {
        (Some(candidate), Some(root)) => candidate == root,
        _ => false,
    }
}

#[cfg(test)]
mod tests;
