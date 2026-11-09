//! Action attribution for detection-stage ask cards. Maps raw shell command
//! constructs to the concrete action taken — publishing, overwriting history,
//! deleting files — so the user decides about the risk they are approving
//! rather than an abstract category that hides it. The sentence derives from
//! the same scans the validators use, so the card and the decision stay in
//! one vocabulary.

use super::detection::expand_inline_interpreter;
use crate::compound::{is_attestable, is_effect_safe, starts_with_supported_command};
use crate::heredoc::strip_quoted_heredoc_bodies;

/// Parse the words of a shell command segment, stepping over leading shell
/// structure words: loop and conditional keywords, case labels, and env
/// assignments.
fn parse_command_words(segment: &str) -> Vec<&str> {
    let raw: Vec<&str> = segment.split_whitespace().collect();
    let mut i = 0;
    while i < raw.len() {
        let w = raw[i].trim_matches(|c: char| c == '"' || c == '\'');
        if w.is_empty() {
            i += 1;
            continue;
        }
        let bare = w.trim_end_matches(';');
        if matches!(
            bare,
            "do" | "done" | "then" | "else" | "elif" | "fi" | "esac" | "break" | "continue"
        ) {
            i += 1;
            continue;
        }
        if matches!(bare, "for" | "while" | "until" | "case" | "if") {
            i += 1;
            let opener = if bare == "case" { "in" } else { "do" };
            while i < raw.len() && raw[i].trim_matches(|c| c == '"' || c == '\'') != opener {
                i += 1;
            }
            if i < raw.len() {
                i += 1;
            }
            continue;
        }
        if let Some((name, _)) = w.split_once('=')
            && !name.is_empty()
            && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            i += 1;
            continue;
        }
        if (w.ends_with(')') && !w.starts_with('('))
            || (bare.starts_with('*') && bare.ends_with('*'))
        {
            i += 1;
            continue;
        }
        break;
    }
    raw[i..]
        .iter()
        .map(|w| w.trim_matches(|c: char| c == '"' || c == '\''))
        .filter(|w| !w.is_empty())
        .collect()
}

/// Find the git subcommand and its trailing args, stepping past git global flags
/// like -C, -c, --git-dir, and options taking a value.
fn git_subcommand_and_args<'a>(words: &'a [&'a str]) -> Option<(&'a str, &'a [&'a str])> {
    let mut idx = 1;
    while idx < words.len() {
        let w = words[idx];
        if matches!(
            w,
            "-C" | "-c" | "--exec-path" | "--namespace" | "--work-tree"
        ) {
            idx += 2;
            continue;
        }
        if w.starts_with("--git-dir=")
            || w.starts_with("--work-tree=")
            || w.starts_with("--namespace=")
            || w.starts_with("-c")
        {
            idx += 1;
            continue;
        }
        if w.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some((w, &words[idx + 1..]));
    }
    None
}

/// The risk sentence for a destructive ask, naming the verb the scan found
/// so the user reads what to decide about rather than a category.
pub(super) fn describe_destructive(content: &str) -> String {
    let expanded = expand_inline_interpreter(content);
    let scan = strip_quoted_heredoc_bodies(&expanded);
    let lower = scan.to_ascii_lowercase();
    for segment in lower.split(['&', ';', '|', '\n', '(', ')', '`']) {
        let words = parse_command_words(segment);
        let Some(&cmd) = words.first() else {
            continue;
        };
        let target = if cmd == "sudo" {
            words.get(1).copied().unwrap_or("sudo")
        } else {
            cmd
        };
        match target {
            "rm" | "rmdir" | "unlink" => {
                return format!("{target} deletes files or directories");
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
    // Prefer a segment that is neither attestable nor effect-safe under a
    // fence, or whose leading command is outside the supported effect set, so
    // a safe temporary redirect is not blamed ahead of an opaque interpreter
    // or binary execution.
    for segment in segments {
        if (!is_attestable(segment) && !is_effect_safe(segment, true))
            || !starts_with_supported_command(segment)
        {
            return clip_step_head(segment);
        }
    }
    for segment in segments {
        if !is_attestable(segment) {
            return clip_step_head(segment);
        }
    }
    segments
        .first()
        .map(|s| clip_step_head(s))
        .unwrap_or_default()
}

fn clip_step_head(segment: &str) -> String {
    let head: String = segment.chars().take(40).collect();
    let mut head = head.trim().to_string();
    if head.chars().count() == 40 {
        head.push('…');
    }
    head
}

pub(super) fn describe_egress_reason(content: &str, outputs_safe: bool) -> String {
    if !outputs_safe {
        return "this command writes a local file; origin approval does not authorize file output"
            .into();
    }
    let expanded = expand_inline_interpreter(content);
    let lower = expanded.to_ascii_lowercase();
    for segment in lower.split(['&', ';', '|', '\n', '(', ')', '`']) {
        let words = parse_command_words(segment);
        let Some(&cmd) = words.first() else {
            continue;
        };
        match cmd {
            "git" => {
                if let Some((sub, args)) = git_subcommand_and_args(&words) {
                    match sub {
                        "push" => {
                            if args.iter().any(|w| {
                                *w == "--force" || *w == "-f" || *w == "--force-with-lease"
                            }) {
                                return "git push --force overwrites history on a remote repository"
                                    .into();
                            }
                            return "git push publishes local commits to a remote repository"
                                .into();
                        }
                        "pull" | "clone" => {
                            return format!(
                                "git {sub} fetches from a remote and modifies the local working tree"
                            );
                        }
                        _ => {}
                    }
                }
            }
            "cargo" | "npm" if words.get(1) == Some(&"publish") => {
                return format!("{cmd} publish publishes a package to an external registry");
            }
            "pip" if words.get(1) == Some(&"upload") => {
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
