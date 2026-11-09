//! The risk sentences the validators put on an ask card. One sentence
//! names the action a command takes — publishing, overwriting history,
//! deleting files — so the user decides about the risk they are approving
//! rather than a category that hides it. The sentence derives from the
//! same scans the validators use, so the card and the decision stay in
//! one vocabulary.

use super::detection::expand_inline_interpreter;
use crate::compound::is_attestable;
use crate::heredoc::strip_quoted_heredoc_bodies;

/// The risk sentence for a destructive ask, naming the verb the scan found
/// so the user reads what to decide about rather than a category.
pub(super) fn describe_destructive(content: &str) -> String {
    let expanded = expand_inline_interpreter(content);
    let scan = strip_quoted_heredoc_bodies(&expanded);
    let lower = scan.to_ascii_lowercase();
    for word in lower.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        match word {
            "rm" | "rmdir" | "unlink" => {
                return format!("{word} deletes files or directories");
            }
            "sudo" => return "sudo runs a command with elevated privileges".into(),
            "dd" => return "dd writes raw bytes to a device or file".into(),
            "truncate" => return "truncate shrinks or empties a file".into(),
            _ => {}
        }
    }
    "destructive pattern needs confirmation".into()
}

/// The first step of a compound the gate cannot verify, named so the user
/// reads which step to decide about. The step text is trimmed to the head
/// of the segment, keeping the card to one line.
pub(super) fn compound_unverified_step(segments: &[&str]) -> String {
    for segment in segments {
        if !is_attestable(segment) {
            let head: String = segment.chars().take(40).collect();
            let mut head = head.trim().to_string();
            if head.chars().count() == 40 {
                head.push('…');
            }
            return head;
        }
    }
    segments
        .first()
        .map(|s| s.chars().take(40).collect())
        .unwrap_or_default()
}

pub(super) fn describe_egress_reason(content: &str, outputs_safe: bool) -> String {
    if !outputs_safe {
        return "this command writes a local file; origin approval does not authorize file output"
            .into();
    }
    let expanded = expand_inline_interpreter(content);
    let lower = expanded.to_ascii_lowercase();
    for segment in lower.split(['&', ';', '|', '\n', '(', ')', '`']) {
        let words: Vec<&str> = segment
            .split_whitespace()
            .filter(|w| !w.contains('=') || w.starts_with('-'))
            .collect();
        let Some(&cmd) = words.first() else {
            continue;
        };
        let sub = words.get(1).copied().unwrap_or("");
        match cmd {
            "git" => match sub {
                "push" => {
                    if words.iter().any(|w| *w == "--force" || *w == "-f") {
                        return "git push --force overwrites history on a remote repository".into();
                    }
                    return "git push publishes local commits to a remote repository".into();
                }
                "pull" | "clone" => {
                    return format!(
                        "git {sub} fetches from a remote and modifies the local working tree"
                    );
                }
                _ => {}
            },
            "cargo" | "npm" if sub == "publish" => {
                return format!("{cmd} publish publishes a package to an external registry");
            }
            "pip" if sub == "upload" => {
                return "pip upload publishes a package to an external registry".into();
            }
            "ssh" | "scp" | "sftp" | "rsync" => {
                return format!("{cmd} opens a remote host connection or file transfer");
            }
            "curl" | "wget" => {
                let sends = words.iter().any(|w| {
                    w.starts_with("-x")
                        || w.starts_with("--request")
                        || w.starts_with("-d")
                        || w.starts_with("--data")
                        || w.starts_with("-T")
                        || w.starts_with("--upload-file")
                });
                if sends {
                    return format!("{cmd} sends data to a remote endpoint");
                }
            }
            "gh" => {}
            _ => {}
        }
    }
    "this command reaches the network".into()
}
