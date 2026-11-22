//! Read-only inspection rules for protected configuration markers and
//! metadata-only probes on credential paths.

use houyicoder_api::sandbox::{fence_covers_all_hits, names_credential_path};
use houyicoder_api::shell::heredoc::strip_quoted_heredoc_bodies;

use super::protected_write::writes_fence_denied_path;
use super::readonly::{
    command_reads_only, expand_readonly_clauses, strip_readonly_constructs, substitutions_read_only,
};
use super::{
    decode_shell_word, is_attestable, is_env_assignment, quotes_balanced, shell_word_spans,
    split_compound, strip_safe_redirects,
};
use crate::safety::marker_hit;

/// Allowed unary file-attribute flags on test or bracket commands that inspect
/// inode metadata without reading file bytes.
const METADATA_TEST_FLAGS: &[&str] = &[
    "-e", "-f", "-d", "-r", "-w", "-x", "-s", "-L", "-h", "-O", "-G", "-p", "-S", "-b", "-c",
];

/// Whether a shell command that references a protected marker only inspects it
/// with read-only commands and writes nothing to a protected path.
#[cfg(test)]
pub fn reads_only_protected(command: &str) -> bool {
    let stripped = strip_quoted_heredoc_bodies(command);
    if !quotes_balanced(&stripped) {
        return false;
    }
    let raw_segments = split_compound(&stripped);
    let refs: Vec<&str> = raw_segments.iter().map(String::as_str).collect();
    let Some(segments) = expand_readonly_clauses(&refs) else {
        return false;
    };
    let mut saw_protected = false;
    for segment in &segments {
        if !marker_hit(segment) {
            continue;
        }
        saw_protected = true;
        if !is_readonly_protected_segment(segment) {
            return false;
        }
    }
    saw_protected
}

/// Whether a shell command reads bytes from a credential file or directory,
/// excluding literal echo banners and metadata-only permission bit checks.
pub(crate) fn reads_credential_content(command: &str) -> bool {
    let stripped = strip_quoted_heredoc_bodies(command);
    if !names_credential_path(&stripped) {
        return false;
    }
    if !quotes_balanced(&stripped) {
        return true;
    }
    let raw_segments = split_compound(&stripped);
    let refs: Vec<&str> = raw_segments.iter().map(String::as_str).collect();
    let Some(segments) = expand_readonly_clauses(&refs) else {
        return true;
    };
    segments
        .iter()
        .any(|seg| names_credential_path(seg) && !is_metadata_only_credential_segment(seg))
}

fn is_readonly_protected_segment(segment: &str) -> bool {
    let cleaned = strip_safe_redirects(segment);
    let Some((rewritten, bodies)) = strip_readonly_constructs(&cleaned) else {
        return false;
    };
    if bodies.iter().any(|body| reads_credential_content(body)) || !substitutions_read_only(&bodies)
    {
        return false;
    }
    let mut words: Vec<String> = Vec::new();
    for raw in shell_word_spans(&rewritten) {
        if is_env_assignment(raw) {
            continue;
        }
        let Some(decoded) = decode_shell_word(raw) else {
            return false;
        };
        words.push(decoded);
    }
    let Some(cmd) = words.first().map(String::as_str) else {
        return false;
    };
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    if cmd == "cd" {
        return bodies.is_empty()
            && args.len() <= 1
            && args.iter().all(|a| !a.contains(['$', '`', '>', '<']));
    }
    command_reads_only(cmd, &args, &rewritten, false)
}

/// Whether every expanded segment touching a protected or credential path
/// is independently cleared, either by static read-only proof (or literal
/// output / metadata-only check) or by the kernel write fence covering every
/// protected marker in that segment without writing it.
pub(crate) fn protected_segments_waived(command: &str, fence_denies_control: bool) -> bool {
    let stripped = strip_quoted_heredoc_bodies(command);
    if !quotes_balanced(&stripped) {
        return false;
    }
    let raw_segments = split_compound(&stripped);
    let refs: Vec<&str> = raw_segments.iter().map(String::as_str).collect();
    let Some(segments) = expand_readonly_clauses(&refs) else {
        return false;
    };
    segments.iter().all(|segment| {
        if names_credential_path(segment) && !is_metadata_only_credential_segment(segment) {
            return false;
        }
        if !marker_hit(segment) {
            return true;
        }
        is_readonly_protected_segment(segment)
            || (fence_denies_control
                && fence_covers_all_hits(segment)
                && !writes_fence_denied_path(segment))
    })
}

/// Whether a segment mentioning a credential path only prints literal text or
/// inspects directory entries and inode metadata without reading file contents.
pub(crate) fn is_literal_output_segment(segment: &str) -> bool {
    let cleaned = strip_safe_redirects(segment);
    if !is_attestable(&cleaned) {
        return false;
    }
    let mut words: Vec<String> = Vec::new();
    for raw in shell_word_spans(&cleaned) {
        if is_env_assignment(raw) {
            return false;
        }
        let Some(decoded) = decode_shell_word(raw) else {
            return false;
        };
        words.push(decoded);
    }
    let Some(cmd) = words.first().map(String::as_str) else {
        return false;
    };
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    match cmd {
        "echo" => true,
        "printf" => !args.iter().any(|a| *a == "-v" || a.starts_with("-v")),
        _ => false,
    }
}

/// Whether a segment mentioning a credential path only prints literal text or
/// inspects directory entries and inode metadata without reading file bytes.
fn is_metadata_only_credential_segment(segment: &str) -> bool {
    if is_literal_output_segment(segment) {
        return true;
    }
    let cleaned = strip_safe_redirects(segment);
    if !is_attestable(&cleaned) {
        return false;
    }
    let mut words: Vec<String> = Vec::new();
    for raw in shell_word_spans(&cleaned) {
        if is_env_assignment(raw) {
            return false;
        }
        let Some(decoded) = decode_shell_word(raw) else {
            return false;
        };
        words.push(decoded);
    }
    let Some(cmd) = words.first().map(String::as_str) else {
        return false;
    };
    let args: Vec<&str> = words[1..].iter().map(String::as_str).collect();
    match cmd {
        "ls" | "stat" => true,
        "test" => args_are_unary_metadata_checks(&args),
        "[" => {
            let Some((&"]", inner)) = args.split_last() else {
                return false;
            };
            args_are_unary_metadata_checks(inner)
        }
        _ => false,
    }
}

fn args_are_unary_metadata_checks(mut args: &[&str]) -> bool {
    while args.first() == Some(&"!") {
        args = &args[1..];
    }
    matches!(args, [flag, path] if METADATA_TEST_FLAGS.contains(flag) && !path.is_empty() && !path.starts_with('-'))
}
