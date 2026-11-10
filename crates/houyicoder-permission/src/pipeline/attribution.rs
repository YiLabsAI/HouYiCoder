//! Action attribution for detection-stage ask cards. Maps raw shell command
//! constructs to the concrete action taken — publishing, overwriting history,
//! deleting files — so the user decides about the risk they are approving
//! rather than an abstract category that hides it. The sentence derives from
//! the same scan the validators use, so the card and the decision stay in one
//! vocabulary.

use crate::compound::{is_attestable, is_effect_safe, starts_with_supported_command};
use crate::shell::{ShellCommand, canonical_scan};

/// The risk sentence for a destructive ask, naming the verb the scan found
/// so the user reads what to decide about rather than a category. An escalated
/// call names the launcher and the verb together: the privilege is the risk,
/// and the verb is what the risk is taken for.
pub(super) fn describe_destructive(content: &str) -> String {
    let scan = canonical_scan(content);
    for command in scan.commands() {
        let predicate = deletion_predicate(command.program());
        let Some(launcher) = command.escalated() else {
            match (command.program(), predicate) {
                ("su" | "sudo", _) => {
                    return format!(
                        "{} runs a command with elevated privileges",
                        command.program()
                    );
                }
                (program, Some(predicate)) => return format!("{program} {predicate}"),
                _ => continue,
            }
        };
        let Some(predicate) = predicate else {
            return format!("{launcher} runs a command with elevated privileges");
        };
        return format!("{launcher} runs {}, which {predicate}", command.program());
    }
    // No named invocation carries the risk: the code this scan could not
    // read does, and the card says so rather than naming a verb it never saw.
    if scan.opaque_code() {
        return "code this scan cannot read needs confirmation".into();
    }
    "destructive pattern needs confirmation".into()
}

/// What the program does to files, as the predicate a sentence completes with.
fn deletion_predicate(program: &str) -> Option<&'static str> {
    match program {
        "rm" | "rmdir" | "unlink" => Some("deletes files or directories"),
        "dd" => Some("writes raw bytes to a device or file"),
        "truncate" => Some("shrinks or empties a file"),
        _ => None,
    }
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
    let scan = canonical_scan(content);
    for command in scan.commands() {
        if let Some(reason) = git_reason(command) {
            return reason;
        }
        let program = command.program();
        match program {
            "cargo" | "npm" if names_word(command, 1, "publish") => {
                return format!("{program} publish publishes a package to an external registry");
            }
            "pip" if names_word(command, 1, "upload") => {
                return "pip upload publishes a package to an external registry".into();
            }
            "ssh" | "scp" | "sftp" | "rsync" => {
                return format!("{program} opens a remote host connection or file transfer");
            }
            "curl" | "wget" if sends_data(command) => {
                return format!("{program} sends data to a remote endpoint");
            }
            _ => {}
        }
    }
    "this command reaches the network".into()
}

/// The sentence for a git call that reaches a remote.
fn git_reason(command: &ShellCommand) -> Option<String> {
    let (sub, args) = command.git_subcommand()?;
    match sub.to_ascii_lowercase().as_str() {
        "push" => {
            if args
                .iter()
                .any(|word| matches!(word.as_str(), "-f" | "--force" | "--force-with-lease"))
            {
                return Some("git push --force overwrites history on a remote repository".into());
            }
            Some("git push publishes local commits to a remote repository".into())
        }
        "pull" | "clone" => Some(format!(
            "git {sub} fetches from a remote and modifies the local working tree"
        )),
        _ => None,
    }
}

/// Short flags that carry a request body out, one letter each. Case matters:
/// the request-method flag is upper case while the proxy flag is lower.
const CURL_DATA_SHORT: &[&str] = &["-X", "-d", "-F", "-T"];

/// Long flags under which a request program sends a body, an upload, or a
/// method of its own choosing.
const CURL_DATA_LONG: &[&str] = &["--request", "--data", "--form", "--upload-file"];
const WGET_DATA_LONG: &[&str] = &[
    "--method",
    "--post-data",
    "--post-file",
    "--body-data",
    "--body-file",
];

/// Whether a request program's arguments carry data out: a body, a form, an
/// upload, or a write method. The programs spell these differently, and a
/// short flag that means a retry count for one means data for the other, so
/// each program is read against its own list.
fn sends_data(command: &ShellCommand) -> bool {
    let program = command.program().to_ascii_lowercase();
    let (short, long) = if program == "wget" {
        (&[][..], WGET_DATA_LONG)
    } else {
        (CURL_DATA_SHORT, CURL_DATA_LONG)
    };
    command.args().iter().any(|word| {
        short.iter().any(|flag| carries_short_flag(word, flag))
            || long
                .iter()
                .any(|flag| word.to_ascii_lowercase().starts_with(flag))
    })
}

/// Whether the word is the flag itself or the flag with a value glued on. A
/// longer word that merely starts with the same letters is another flag.
fn carries_short_flag(word: &str, flag: &str) -> bool {
    word.strip_prefix(flag)
        .is_some_and(|rest| rest.is_empty() || !rest.starts_with(|c: char| c.is_ascii_alphabetic()))
}

/// Whether the word at an index is the named word, compared without case.
fn names_word(command: &ShellCommand, index: usize, name: &str) -> bool {
    command
        .word(index)
        .is_some_and(|word| word.eq_ignore_ascii_case(name))
}
