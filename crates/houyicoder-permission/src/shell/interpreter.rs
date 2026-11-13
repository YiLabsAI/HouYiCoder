//! What code a program is handed, and what this scan can do with it.
//!
//! An interpreter takes its code from a flag on the command line, from a file,
//! or from standard input. This module reads the flag: which options carry
//! inline code for each program this scan knows, and where the code sits in a
//! word the shell may have split or glued. The reader a program names decides
//! how the code is judged, so the same question answers both.

use houyicoder_api::shell::interpreter::is_read_only_program;

use super::ShellCommand;
use super::ShellScan;
use super::powershell::read_only as powershell_read_only;
use super::program_name;
use super::scan_text;

/// Judge the code an interpreter is handed: a shell's body is more shell to
/// scan, a Python body is judged by the read-only rule, and code in any other
/// language is code this scan cannot vouch for.
pub(super) fn classify_interpreter_input(
    program: &str,
    code: &str,
    depth: u8,
    escalation: Option<&str>,
    scan: &mut ShellScan,
) {
    // PowerShell has its own read-only classifier; it is not scanned as posix
    // shell text, which would misread its syntax.
    let name = program_name(program).to_ascii_lowercase();
    if name == "powershell" || name == "pwsh" {
        scan.codes.push(code.to_string());
        if !powershell_read_only(code) {
            scan.opaque_code = true;
        }
        return;
    }
    match code_reader(program) {
        Some(CodeReader::Shell) => {
            if depth > 1 {
                scan_text(code, depth - 1, escalation, scan);
            } else {
                scan.opaque_code = true;
            }
        }
        Some(CodeReader::Python) => {
            scan.codes.push(code.to_string());
            if !is_read_only_program(code) {
                scan.opaque_code = true;
            }
        }
        Some(CodeReader::Opaque) | None => scan.opaque_code = true,
    }
}

/// What this scan can do with the code a program is handed.
pub(super) enum CodeReader {
    /// A shell: its code is more shell text, walked by this scan.
    Shell,
    /// The Python family: its code is judged by the read-only rule.
    Python,
    /// A language whose code this scan does not read.
    Opaque,
}

/// The code reader a program names, or None when it is not an interpreter.
pub(super) fn code_reader(program: &str) -> Option<CodeReader> {
    let name = program_name(program).to_ascii_lowercase();
    let reader = match name.as_str() {
        "bash" | "cmd" | "dash" | "fish" | "ksh" | "powershell" | "pwsh" | "script" | "sh"
        | "su" | "zsh" => CodeReader::Shell,
        "python" | "python2" | "python3" => CodeReader::Python,
        "expect" | "node" | "osascript" | "perl" | "ruby" | "tclsh" => CodeReader::Opaque,
        _ => return None,
    };
    Some(reader)
}

/// Whether the program runs the shell code it is handed.
pub(super) fn shell_interpreter(program: &str) -> bool {
    matches!(code_reader(program), Some(CodeReader::Shell))
}

/// The options that introduce code a program runs.
struct InlineOptions {
    /// A short option letter. A cluster of short options may carry it, with
    /// the code glued after the letter or in the word after the cluster.
    short: Option<char>,
    /// The long options that take the code as their value.
    long: &'static [&'static str],
    /// Whole words that introduce the code as the following word.
    words: &'static [&'static str],
}

/// The options carrying inline code, for the interpreters that take it inline.
/// A file argument is not inline code: the file is what other checks read.
fn inline_options(program: &str) -> Option<InlineOptions> {
    let name = program_name(program).to_ascii_lowercase();
    let short_only = |short: char| InlineOptions {
        short: Some(short),
        long: &[],
        words: &[],
    };
    match code_reader(program)? {
        CodeReader::Shell => Some(match name.as_str() {
            "cmd" => InlineOptions {
                short: None,
                long: &[],
                words: &["/c"],
            },
            "powershell" | "pwsh" => InlineOptions {
                short: Some('c'),
                long: &[],
                words: &["-command"],
            },
            _ => short_only('c'),
        }),
        CodeReader::Python => Some(short_only('c')),
        CodeReader::Opaque => match name.as_str() {
            "node" | "perl" | "ruby" => Some(short_only('e')),
            _ => None,
        },
    }
}

/// Whether the program takes code from a flag on its command line. A heredoc
/// beside such a program is not the code that runs.
pub(super) fn takes_inline_code(program: &str) -> bool {
    inline_options(program).is_some()
}

/// Where an option's value sits.
enum OptionValue<'a> {
    /// The value is the word after the option.
    Next,
    /// The value is glued to the option in the same word.
    Glued(&'a str),
}

/// The code an invocation carries on its command line, read from its flag
/// however the shell word-split it: in a separate word, in a cluster of short
/// options, or glued to the flag letter.
pub(super) fn inline_code(command: &ShellCommand) -> Option<String> {
    let options = inline_options(command.program())?;
    let args = command.args();
    for (index, word) in args.iter().enumerate() {
        let found = if options
            .words
            .iter()
            .any(|name| word.eq_ignore_ascii_case(name))
        {
            Some(OptionValue::Next)
        } else if let Some(found) = long_option_value(word, options.long) {
            Some(found)
        } else if let Some(short) = options.short {
            short_option_value(word, short)
        } else {
            None
        };
        match found {
            Some(OptionValue::Next) => return args.get(index + 1).cloned(),
            Some(OptionValue::Glued(code)) => return Some(code.to_string()),
            None => {}
        }
    }
    None
}

/// The value a word carries for a long option, either as the next word or
/// glued after an equals sign.
fn long_option_value<'a>(word: &'a str, names: &[&str]) -> Option<OptionValue<'a>> {
    let (name, glued) = match word.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (word, None),
    };
    if !names
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
    {
        return None;
    }
    Some(match glued {
        Some(value) => OptionValue::Glued(value),
        None => OptionValue::Next,
    })
}

/// The value a word carries for a short option. A word holding a cluster of
/// short options carries the value glued after the letter, or takes the next
/// word when the letter ends the cluster.
fn short_option_value(word: &str, short: char) -> Option<OptionValue<'_>> {
    let letters = word.strip_prefix('-')?;
    if letters.is_empty() || letters.starts_with('-') {
        return None;
    }
    let at = letters.find(short)?;
    let rest = &letters[at + short.len_utf8()..];
    if rest.is_empty() {
        Some(OptionValue::Next)
    } else {
        Some(OptionValue::Glued(rest))
    }
}
