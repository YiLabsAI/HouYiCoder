//! Whether a shell command writes a path the execution fence refuses.
//!
//! The evidence is structural: an output redirect whose target is such a path,
//! or a writing command naming one in a position it writes. Text handed to an
//! interpreter is judged only when that interpreter runs it, because a quoted
//! word the shell never reads as a command would otherwise read as a write.

use super::{decode_shell_word, is_env_assignment, shell_word_spans, split_compound};
use crate::heredoc::quoted_heredoc_bodies;
use crate::heredoc::strip_quoted_heredoc_bodies;
use crate::safety::fence_denied_path;

/// The commands whose own operands name the paths they write. Without this
/// list an argument is only evidence that a word was used, so a command that
/// merely quotes a control path in its message would read as a write to it.
/// A copy or a link is not in the list: only its destination is written, and
/// its source is read, so the position of the operand decides whether the
/// command touches the path at all.
const WRITING_COMMANDS: &[&str] = &[
    "chgrp", "chmod", "chown", "mkdir", "mkfifo", "mv", "rm", "rmdir", "shred", "tee", "touch",
    "truncate", "unlink",
];

/// Programs that run the text handed to them as code.
const SCRIPT_RUNNERS: &[&str] = &[
    "bash", "dash", "node", "perl", "php", "python", "python3", "ruby", "sh", "zsh",
];

/// Calls and methods with which a script writes the path it names. A move and
/// a replacement change both of their operands, so either one counts; a copy
/// writes only its destination and is handled separately.
const SCRIPT_WRITE_CALLS: &[&str] = &[
    ".write_bytes(",
    ".write_text(",
    "os.chmod(",
    "os.remove(",
    "os.rename(",
    "os.replace(",
    "os.unlink(",
    "shutil.move(",
    "shutil.rmtree(",
];

/// Copy calls, which read their source and write the destination they name
/// last. Matching them beside the calls above would call a backup of a
/// control file a write to it.
const SCRIPT_COPY_CALLS: &[&str] = &["shutil.copy(", "shutil.copy2(", "shutil.copyfile("];

/// Whether some segment of a command writes a path the execution fence
/// refuses, or an interpreter runs text that writes one. A quote-insensitive
/// search over the whole line cannot answer this: the path also appears in a
/// commit message or in a string redirected elsewhere, and both are ordinary
/// commands whose answer should be the user's.
pub(crate) fn writes_fence_denied_path(command: &str) -> bool {
    let stripped = strip_quoted_heredoc_bodies(command);
    if split_compound(&stripped)
        .iter()
        .any(|segment| segment_writes_denied_path(segment))
    {
        return true;
    }
    // A script handed to an interpreter is one quoted word or a heredoc body,
    // so the write it performs is inside text the shell never reads as a
    // command. Judging that text only when an interpreter runs it keeps a
    // commit message or a note from being read as a write.
    script_writes_denied_path(command, &stripped)
}

fn segment_writes_denied_path(segment: &str) -> bool {
    if output_redirect_targets(segment)
        .iter()
        .any(|t| fence_denied_path(t))
    {
        return true;
    }
    let words = positional_words(segment);
    let Some(Some(command)) = words.first() else {
        return false;
    };
    match command.as_str() {
        // The source of a copy or a link is read; only the destination is
        // written. A backup of a control file is the user's own business.
        "cp" | "install" | "ln" => destination(&words).is_some_and(names_denied),
        // A move takes the source away as well, so both ends change.
        "mv" => words[1..].iter().any(names_denied),
        // dd names its destination in its own of= operand, while its if=
        // operand is a read, so only the destination counts.
        "dd" => words.iter().any(|word| {
            word.as_deref()
                .and_then(|word| word.strip_prefix("of="))
                .is_some_and(fence_denied_path)
        }),
        // An in-place edit rewrites its file operands; the program text is not
        // one of them, whether it arrives through an option or as the first
        // operand, and the same rule holds in the long and short spellings.
        "sed" | "perl" | "ruby" if rewrites_in_place(&words) => {
            let scan = scan_operands(&words, &[]);
            let mut names = scan.names.into_iter();
            if !scan.text_from_option {
                names.next();
            }
            names.any(names_denied)
        }
        // The first operand is the mode or the owner, and the names after it
        // are written. A reference file supplies that first operand from a
        // read instead, so with one present every operand is a name.
        "chmod" | "chown" | "chgrp" => {
            let scan = scan_operands(&words, &["--reference"]);
            let mut names = scan.names.into_iter();
            if !scan.read_from_option {
                names.next();
            }
            names.any(names_denied)
        }
        _ => WRITING_COMMANDS.contains(&command.as_str()) && words[1..].iter().any(names_denied),
    }
}

/// Whether an operand is a decoded word that names a fence-denied path. A
/// word the scan could not decode holds an expansion, so it cannot be shown
/// to name the path.
fn names_denied(word: &Option<String>) -> bool {
    word.as_deref().is_some_and(fence_denied_path)
}

/// The words of one segment in their operand positions, with quotes and
/// escapes resolved, and leading environment assignments dropped. A word that
/// cannot be decoded keeps its position as None, because dropping it would
/// move the next operand into the place the check reads.
fn positional_words(segment: &str) -> Vec<Option<String>> {
    shell_word_spans(segment)
        .into_iter()
        .filter(|raw| !is_env_assignment(raw))
        .map(decode_shell_word)
        .collect()
}

/// The words of one segment with quotes and escapes resolved, and leading
/// environment assignments dropped. Used where only the command name matters.
fn decoded_words(segment: &str) -> Vec<String> {
    positional_words(segment).into_iter().flatten().collect()
}

/// The destination of a copy or a link: the operand after an explicit target
/// directory option, or the last operand otherwise. Options are skipped so a
/// flag's value is not mistaken for the destination.
fn destination(words: &[Option<String>]) -> Option<&Option<String>> {
    if let Some(at) = words.iter().position(|word| {
        word.as_deref()
            .is_some_and(|w| w == "-t" || w == "--target-directory")
    }) {
        return words.get(at + 1);
    }
    words
        .iter()
        .skip(1)
        .rev()
        .find(|word| !word.as_deref().is_some_and(|w| w.starts_with('-')))
}

/// The options whose value is program text, so the word after one is not a
/// name the command writes.
const TEXT_OPTIONS: &[&str] = &["--expression", "--file", "-e", "-f"];

/// The operands of a command, and where the values that pushed them out of
/// first position came from.
struct Operands<'a> {
    /// The words in operand positions, in order.
    names: Vec<&'a Option<String>>,
    /// Whether an option supplied the program text, so the first operand is
    /// not that text.
    text_from_option: bool,
    /// Whether an option supplied a file the command reads, so the first
    /// operand is not the mode or the owner.
    read_from_option: bool,
}

/// Split the words after the command name into operands and options, dropping
/// the value an option takes with it. The options in reads name a file the
/// command only reads; the expression options carry program text. A word that
/// cannot be decoded keeps its operand position, since dropping it would move
/// the next word into the place the caller reads.
fn scan_operands<'a>(words: &'a [Option<String>], reads: &[&str]) -> Operands<'a> {
    let mut names = Vec::new();
    let mut text_from_option = false;
    let mut read_from_option = false;
    let mut options_done = false;
    let mut skip_value = false;
    for word in words.iter().skip(1) {
        if skip_value {
            skip_value = false;
            continue;
        }
        if let Some(text) = word.as_deref().filter(|_| !options_done) {
            if text == "--" {
                options_done = true;
                continue;
            }
            if TEXT_OPTIONS.contains(&text) {
                text_from_option = true;
                skip_value = true;
                continue;
            }
            if carries_attached_text(text) {
                text_from_option = true;
                continue;
            }
            if reads.contains(&text) {
                read_from_option = true;
                skip_value = true;
                continue;
            }
            if reads.iter().any(|read| {
                text.strip_prefix(read)
                    .is_some_and(|rest| rest.starts_with('='))
            }) {
                read_from_option = true;
                continue;
            }
            if text.starts_with('-') && text.len() > 1 {
                continue;
            }
        }
        names.push(word);
    }
    Operands {
        names,
        text_from_option,
        read_from_option,
    }
}

/// Whether a short option carries its program text in the same word. The text
/// is attached to the letter naming the option, so the rest of the word is not
/// an operand.
fn carries_attached_text(word: &str) -> bool {
    let Some(rest) = word.strip_prefix('-') else {
        return false;
    };
    !rest.starts_with('-') && matches!(rest.chars().next(), Some('e') | Some('f'))
}

/// Whether the edit rewrites its files in place, in either spelling. Without
/// it the command only reads its operands to report on them.
fn rewrites_in_place(words: &[Option<String>]) -> bool {
    words.iter().skip(1).any(|word| {
        word.as_deref().is_some_and(|text| {
            text == "--in-place"
                || text.starts_with("--in-place=")
                || short_cluster_rewrites_in_place(text)
        })
    })
}

/// Whether a short option cluster carries the in-place letter. Letters are
/// read left to right, and a letter that takes a value ends the cluster: the
/// rest of the word is that value, which may contain any text.
fn short_cluster_rewrites_in_place(word: &str) -> bool {
    let Some(rest) = word.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('-') {
        return false;
    }
    rest.chars()
        .take_while(|letter| !matches!(letter, 'e' | 'f' | 'l'))
        .any(|letter| letter == 'i')
}

/// Whether text an interpreter runs names a fence-denied path and writes it.
/// The path is named either on the line that writes or by a variable the
/// script binds to it earlier: a cleanup that assigns the settings file to a
/// name and replaces that name writes the file, while a script that names the
/// file only to read it does not. A binding carries no further inference, so a
/// name computed from the path -- a temporary sibling -- is not the path.
fn script_writes_denied_path(command: &str, stripped: &str) -> bool {
    let runs_script = split_compound(stripped).iter().any(|segment| {
        decoded_words(segment)
            .first()
            .is_some_and(|word| SCRIPT_RUNNERS.contains(&word.as_str()))
    });
    if !runs_script {
        return false;
    }
    let bodies = quoted_heredoc_bodies(command);
    let lines: Vec<&str> = stripped
        .lines()
        .chain(bodies.iter().flat_map(|body| body.lines()))
        .collect();
    let bound = bound_to_denied_path(&lines);
    lines.iter().any(|line| script_line_writes(line, &bound))
}

/// The script variables assigned from an expression that names a fence-denied
/// path, so a later write naming one of them writes that path.
fn bound_to_denied_path(lines: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in lines {
        let Some((name, value)) = binding(line) else {
            continue;
        };
        if fence_denied_path(value) && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// The variable a script line binds and the expression it binds it to: an
/// assignment or the header of a loop. A comparison and an augmented
/// assignment bind nothing new.
fn binding(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let (name, value) = if let Some(header) = trimmed.strip_prefix("for ") {
        header.split_once(" in ")?
    } else {
        let (name, value) = trimmed.split_once('=')?;
        if value.starts_with('=') || name.ends_with(['+', '-', '*', '/', '%', '!', '<', '>']) {
            return None;
        }
        (name, value)
    };
    let name = name.trim();
    is_script_identifier(name).then_some((name, value))
}

/// Whether a word can be a script variable name.
fn is_script_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether one line of a script writes a fence-denied path. A copy counts only
/// when its destination operand is the path, matching the shell rule.
fn script_line_writes(line: &str, bound: &[String]) -> bool {
    if let Some(destination) = SCRIPT_COPY_CALLS
        .iter()
        .find_map(|call| copy_destination(line, call))
        && names_denied_path(destination, bound)
    {
        return true;
    }
    let writes = SCRIPT_WRITE_CALLS.iter().any(|call| line.contains(call)) || opens_for_write(line);
    writes && names_denied_path(line, bound)
}

/// Whether the text names a fence-denied path itself or through one of the
/// variables bound to one.
fn names_denied_path(text: &str, bound: &[String]) -> bool {
    fence_denied_path(text) || bound.iter().any(|name| names_identifier(text, name))
}

/// The destination operand of a copy call on this line, when the call and its
/// closing parenthesis both sit on the line.
fn copy_destination<'a>(line: &'a str, call: &str) -> Option<&'a str> {
    let start = line.find(call)? + call.len();
    let close = line[start..].find(')')? + start;
    let (_, destination) = line[start..close].rsplit_once(',')?;
    Some(destination.trim())
}

/// Whether the text contains the name as a whole word, so a longer name that
/// carries it does not count.
fn names_identifier(text: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(at) = text[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let opens = !text[..start].chars().next_back().is_some_and(is_name_char);
        let closes = !text[end..].chars().next().is_some_and(is_name_char);
        if opens && closes {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Whether the character can continue a script name.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether a script line opens the path it names for writing. The mode string
/// is the only evidence the line carries, so both the call and a write mode
/// have to appear on it.
fn opens_for_write(line: &str) -> bool {
    (line.contains("open(") || line.contains(".open("))
        && [
            "\"w\"", "'w'", "\"a\"", "'a'", "\"w+\"", "\"wb\"", "\"ab\"", "'w+'",
        ]
        .iter()
        .any(|mode| line.contains(mode))
}

/// The words a segment redirects output into, with their quotes resolved. The
/// scan tracks quote state, so an angle bracket or ampersand inside a quoted
/// argument is data. A redirection of a descriptor into another one carries no
/// file name and is skipped.
fn output_redirect_targets(segment: &str) -> Vec<String> {
    let chars: Vec<char> = segment.chars().collect();
    let mut targets = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' => {
                let quote = chars[i];
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    i += 1;
                }
                i += 1;
            }
            '\\' => i += 2,
            '>' if chars.get(i + 1) != Some(&'(') => {
                i += 1;
                if chars.get(i) == Some(&'>') {
                    i += 1;
                }
                while matches!(chars.get(i), Some(' ' | '\t')) {
                    i += 1;
                }
                if chars.get(i) == Some(&'&') {
                    // A descriptor copied into another descriptor names no
                    // file, while this shell's all-output redirect into a word
                    // does, so only a number or a closed descriptor is skipped.
                    if matches!(chars.get(i + 1), Some(c) if c.is_ascii_digit() || *c == '-') {
                        continue;
                    }
                    i += 1;
                    while matches!(chars.get(i), Some(' ' | '\t')) {
                        i += 1;
                    }
                }
                if chars.get(i) == Some(&'|') {
                    i += 1;
                    while matches!(chars.get(i), Some(' ' | '\t')) {
                        i += 1;
                    }
                }
                let (target, next) = read_word(&chars, i);
                i = next;
                if let Some(target) = target.and_then(|raw| decode_shell_word(&raw)) {
                    targets.push(target);
                }
            }
            _ => i += 1,
        }
    }
    targets
}

/// The raw word starting at start, kept with its quotes, and the position just
/// past it. A word ends at whitespace or at a shell operator that cannot be
/// part of a file name.
fn read_word(chars: &[char], start: usize) -> (Option<String>, usize) {
    let mut out = String::new();
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            '\'' | '"' => {
                let quote = chars[i];
                out.push(quote);
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    i += 1;
                    if chars[i - 1] == quote {
                        break;
                    }
                }
            }
            '\\' => {
                out.push(chars[i]);
                if let Some(next) = chars.get(i + 1) {
                    out.push(*next);
                }
                i += 2;
            }
            c if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')') => break,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    let word = (!out.is_empty()).then_some(out);
    (word, i)
}
