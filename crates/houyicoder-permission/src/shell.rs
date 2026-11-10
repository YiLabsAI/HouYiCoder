//! Canonical shell extraction: one preprocessing pipeline that every scan of
//! a shell command shares, so no scan's answer depends on which scan ran.
//!
//! It strips quoted heredoc bodies, classifies the code handed to an
//! interpreter, peels loop and case structure, and steps over the programs
//! that run the command written after their own arguments. A construct the
//! pipeline cannot read is reported rather than guessed at, and the caller
//! decides whether that means asking.

use houyicoder_api::shell::heredoc::{strip_heredoc_bodies, unquoted_heredoc_bodies};

use crate::compound::{split_compound, strip_safe_redirects};

mod interpreter;
mod launcher;
mod reader;
mod unwrapper;

use interpreter::{
    classify_interpreter_input, code_reader, inline_code, shell_interpreter, takes_inline_code,
};
pub(crate) use launcher::is_launcher;
use launcher::{
    argument_programs, container_command_index, is_assignment_word, launcher_escalates,
    runner_command_text, split_string_value, wrapped_index,
};
use reader::{program_bodies, scan_body, scan_pipelines};
pub(crate) use unwrapper::{
    decode_word, has_process_substitution, leading_command_substitution, mark_substitutions,
    split_words_quote_aware, substitution_bodies,
};

/// How many nested interpreter, substitution, and heredoc layers the scan
/// follows before it stops trusting its own reading.
const MAX_DEPTH: u8 = 4;

/// One token as the shell would hand it to a program: the punctuation that
/// can wrap a command in place is trimmed, and quoting and escapes are
/// resolved.
pub(crate) fn decode_token(raw: &str) -> String {
    decode_word(
        raw.trim_start_matches(['(', '{'])
            .trim_end_matches([')', '}']),
    )
}

/// Words the shell reads as control structure rather than as a command.
pub(crate) fn is_shell_control_keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "elif"
            | "else"
            | "fi"
            | "for"
            | "while"
            | "until"
            | "do"
            | "done"
            | "time"
            | "!"
    )
}

/// Words the shell reads as structure rather than as a command. A word that
/// was nothing but grouping punctuation is structure too, which is why an
/// empty word counts.
fn is_structure_word(word: &str) -> bool {
    word.is_empty()
        || matches!(
            word,
            "!" | "{" | "}" | ";;" | "do" | "done" | "elif" | "else" | "esac" | "fi" | "then"
        )
}

/// The name a program is started by: the last path segment, without the
/// executable suffix the platform may add.
pub(crate) fn program_name(word: &str) -> &str {
    let file = word.rsplit(['/', '\\']).next().unwrap_or(word);
    file.strip_suffix(".exe").unwrap_or(file)
}

/// Whether a word starts the named program: the words are compared as the
/// shell resolves them, by bare file name and without case.
pub(crate) fn names_program(word: &str, program: &str) -> bool {
    program_name(word).eq_ignore_ascii_case(program_name(program))
}

/// One program invocation, as the words the shell hands the program.
pub(crate) struct ShellCommand {
    words: Vec<String>,
    /// The launcher that runs this invocation with elevated privileges, when
    /// one stands in front of it. Privilege escalation is a risk of its own,
    /// so it survives the stepping that removes the launcher's own words.
    escalated: Option<String>,
}

impl ShellCommand {
    /// The program at the head of the invocation.
    pub(crate) fn program(&self) -> &str {
        self.words.first().map(String::as_str).unwrap_or_default()
    }

    /// The launcher that runs this invocation with elevated privileges.
    pub(crate) fn escalated(&self) -> Option<&str> {
        self.escalated.as_deref()
    }

    /// The words after the program.
    pub(crate) fn args(&self) -> &[String] {
        &self.words[1..]
    }

    /// The word at an index, counting the program as word zero.
    pub(crate) fn word(&self, index: usize) -> Option<&str> {
        self.words.get(index).map(String::as_str)
    }

    /// Whether the head of the invocation is the named program. The word is
    /// read as the shell starts it: a path to the file, another case, and the
    /// executable suffix all name the same program.
    pub(crate) fn runs(&self, program: &str) -> bool {
        names_program(self.program(), program)
    }

    /// Whether the head of the invocation is any of the named programs.
    pub(crate) fn is_one_of(&self, programs: &[&str]) -> bool {
        programs.iter().any(|program| self.runs(program))
    }

    /// The subcommand of a git invocation and the words after it, stepping
    /// over the global options that take a separate value.
    pub(crate) fn git_subcommand(&self) -> Option<(&str, &[String])> {
        if !self.runs("git") {
            return None;
        }
        let args = self.args();
        let mut index = 0;
        while index < args.len() {
            let word = args[index].as_str();
            if matches!(
                word,
                "-C" | "-c" | "--exec-path" | "--git-dir" | "--namespace" | "--work-tree"
            ) {
                index += 2;
                continue;
            }
            if word.starts_with('-') {
                index += 1;
                continue;
            }
            return Some((word, &args[index + 1..]));
        }
        None
    }
}

/// What one pipeline stage does.
pub(crate) enum Stage {
    /// The stage names no program: shell structure, or leading assignments
    /// that reach no command.
    Empty,
    /// The stage runs a program.
    Program {
        /// The invocation, with wrappers stepped over.
        command: ShellCommand,
        /// Whether every word is one the shell passes as written. An argument
        /// the unwrapper could not read leaves the program right and the
        /// argument list unreliable, so a caller reading a destination out of
        /// the arguments must refuse.
        exact: bool,
    },
}

/// What one shell text runs.
pub(crate) struct ShellScan {
    commands: Vec<ShellCommand>,
    codes: Vec<String>,
    opaque_code: bool,
}

impl ShellScan {
    /// The programs the text runs, in the order the scan found them.
    pub(crate) fn commands(&self) -> &[ShellCommand] {
        &self.commands
    }

    /// The code the text hands to an interpreter outside the shell family, as
    /// the interpreter receives it. A caller whose concern can appear as a
    /// library call reads this text, because such a call is not a shell stage.
    pub(crate) fn codes(&self) -> &[String] {
        &self.codes
    }

    /// Whether the text hands code to something this scan cannot read as a
    /// program that only reads. Such code can do anything a program can, so a
    /// caller judging effect must hold.
    pub(crate) fn opaque_code(&self) -> bool {
        self.opaque_code
    }
}

/// Read a shell command as the programs it runs.
pub(crate) fn canonical_scan(content: &str) -> ShellScan {
    let mut scan = ShellScan {
        commands: Vec::new(),
        codes: Vec::new(),
        opaque_code: false,
    };
    // A heredoc body is literal text to the shell and code to the program
    // reading it, so it is judged by what that program does with it rather
    // than as part of the command line.
    for body in program_bodies(content) {
        scan_body(body, &mut scan);
    }
    scan_text(content, MAX_DEPTH, None, &mut scan);
    scan_pipelines(content, MAX_DEPTH, &mut scan);
    scan
}

/// Read one stage: the program at its head with the words it is passed.
pub(crate) fn stage_program(segment: &str) -> Stage {
    let cleaned = strip_safe_redirects(segment.trim());
    if cleaned.trim().is_empty() {
        return Stage::Empty;
    }
    match unwrap_clause(&cleaned) {
        Some(peeled) => resolve_program(&words_of(&peeled), true),
        // The unwrapper refused the stage: a substitution sits in argument
        // position, or a leading assignment rebinds how the program starts.
        // The head is still the program the shell starts, so it is read from
        // the words as written, and the argument list is marked unreliable.
        None => resolve_program(&loose_words(&cleaned), false),
    }
}

/// The inline code a stage runs through an interpreter, when the stage is one
/// interpreter invocation carrying code on its command line.
pub(crate) fn stage_inline_code(segment: &str) -> Option<String> {
    let Stage::Program { command, .. } = stage_program(segment) else {
        return None;
    };
    inline_code(&command)
}

/// The program a stage runs, once the structure in front of it is dropped.
fn resolve_program(words: &[String], exact: bool) -> Stage {
    let mut words: Vec<String> = words.to_vec();
    let mut index = 0;
    let mut escalated: Option<String> = None;
    while index < words.len() {
        while index < words.len() && is_structure_word(&words[index]) {
            index += 1;
        }
        while index < words.len() && is_assignment_word(&words[index]) {
            index += 1;
        }
        if index >= words.len() {
            break;
        }
        // A launcher option whose value is itself a command line is spliced
        // in, so the words it spells are judged like any other command.
        if let Some((consumed, split)) = split_string_value(&words, index) {
            words.splice(index..index + consumed, split);
            continue;
        }
        // A container runtime's exec subcommand runs the command written
        // after the container, so the words after that operand are the
        // command the runtime starts.
        if let Some(next) = container_command_index(&words, index) {
            index = next;
            continue;
        }
        match wrapped_index(&words, index) {
            Some(next) if next > index => {
                if launcher_escalates(&words[index]) {
                    escalated = Some(words[index].to_ascii_lowercase());
                }
                index = next;
            }
            _ => {
                return Stage::Program {
                    command: ShellCommand {
                        words: words[index..].to_vec(),
                        escalated,
                    },
                    exact,
                };
            }
        }
    }
    Stage::Empty
}

/// Whether a word stands in for a program the shell forms at run time, so no
/// check can name what runs from the text alone.
pub(crate) fn unresolved_program(program: &str) -> bool {
    matches!(program, "CMD" | "CAPTURE") || program.starts_with('$')
}

/// Whether an awk program runs a command of its own: the system call, or a
/// pipe its getline reads. Without either, awk only reads and prints.
fn awk_runs_a_command(command: &ShellCommand) -> bool {
    if !command.is_one_of(&["awk", "gawk", "mawk", "nawk"]) {
        return false;
    }
    let text = command.args().join(" ");
    text.contains("system(") || (text.contains("getline") && text.contains('|'))
}

/// Whether a stage's program is one the shell resolves at run time.
pub(crate) fn program_is_unresolved(segment: &str) -> bool {
    matches!(
        stage_program(segment),
        Stage::Program { command, .. } if unresolved_program(command.program())
    )
}

/// Split text into words with their quoting and escapes resolved. Grouping
/// punctuation that wraps a command in place is trimmed from an unquoted
/// word, so a subshell-wrapped stage still names its program.
fn words_of(text: &str) -> Vec<String> {
    split_words_quote_aware(text)
        .into_iter()
        .map(|word| {
            if word.starts_with(['"', '\'']) {
                decode_word(&word)
            } else {
                let trimmed = word
                    .trim_start_matches(['(', '{'])
                    .trim_end_matches([')', '}']);
                decode_word(trimmed)
            }
        })
        .collect()
}

/// Split text into words without peeling shell structure, so a stage the
/// unwrapper refused still yields the program its head names.
fn loose_words(text: &str) -> Vec<String> {
    let mut words = words_of(text);
    let mut index = 0;
    while index < words.len()
        && (is_structure_word(&words[index]) || is_assignment_word(&words[index]))
    {
        index += 1;
    }
    words.drain(..index);
    words
}

/// Walk one text: its substitutions and its stages.
fn scan_text(content: &str, depth: u8, escalation: Option<&str>, scan: &mut ShellScan) {
    if depth == 0 {
        // Nested shell code this scan refuses to follow is code it cannot
        // vouch for, so a caller judging effect holds.
        scan.opaque_code = true;
        return;
    }
    // A heredoc body is text the shell hands to the reading program, never a
    // line of the command; that program was judged on the body above. The
    // shell does expand an unquoted body first, so the substitutions one
    // carries are commands here.
    for body in unquoted_heredoc_bodies(content) {
        if let Some(bodies) = substitution_bodies(&body) {
            for body in bodies {
                scan_text(&body, depth - 1, escalation, scan);
            }
        }
    }
    let text = strip_heredoc_bodies(content);
    // A command substitution runs its body as a command wherever it sits: in
    // an assignment, in a loop generator, or inside double quotes.
    if let Some(bodies) = substitution_bodies(&text) {
        for body in bodies {
            scan_text(&body, depth - 1, escalation, scan);
        }
    }
    // The bodies were walked above, so their text is replaced by a marker
    // word: a substitution glued to a word would otherwise hide the program
    // the word starts, and the marker keeps the stages around it intact.
    let marked = mark_substitutions(&text).unwrap_or(text);
    for segment in split_compound(&marked) {
        scan_segment(&segment, depth, escalation, scan);
    }
}

/// Walk one stage and record what it runs. An interpreter whose code arrives
/// on standard input was judged by its body, so the invocation itself adds
/// nothing: the body is what runs, not the interpreter's name.
fn scan_segment(segment: &str, depth: u8, escalation: Option<&str>, scan: &mut ShellScan) {
    if let Stage::Program { command, .. } = stage_program(segment) {
        if unresolved_program(command.program()) {
            // The program name is text the shell forms at run time, so no
            // stage-level check can vouch for what runs.
            scan.opaque_code = true;
            return;
        }
        if reads_code_from_input(segment, &command) {
            return;
        }
        if awk_runs_a_command(&command) {
            // The awk program carries a call this scan does not read, so the
            // stage can do whatever that call does.
            scan.opaque_code = true;
        }
        record_command(command, depth, escalation, scan);
    }
}

/// Whether a stage hands an interpreter code through a heredoc, in which case
/// the body classification is the whole answer.
fn reads_code_from_input(segment: &str, command: &ShellCommand) -> bool {
    takes_inline_code(command.program()) && segment.contains("<<") && inline_code(command).is_none()
}

/// Record one program invocation, reading an interpreter's code instead of
/// treating the interpreter as the whole program.
fn record_command(
    mut command: ShellCommand,
    depth: u8,
    escalation: Option<&str>,
    scan: &mut ShellScan,
) {
    if command.escalated.is_none() {
        command.escalated = escalation.map(str::to_string);
    }
    let program = program_name(command.program()).to_ascii_lowercase();
    if program == "eval" {
        // The builtin parses its own arguments as shell text, so words in
        // argument position are commands the shell runs.
        let code = command.args().join(" ");
        if depth > 1 {
            scan_text(&code, depth - 1, command.escalated.as_deref(), scan);
        } else {
            scan.opaque_code = true;
        }
        return;
    }
    if let Some(snippet) = runner_command_text(&command) {
        // A runner hands its arguments to another command, which it also
        // accepts written as shell text. The argument words are read as shell
        // text rather than as a program, so a command written with spaces or
        // a runner option in front of it is still read.
        let escalation = command.escalated.clone();
        scan.commands.push(command);
        if depth > 1 {
            scan_text(&snippet, depth - 1, escalation.as_deref(), scan);
        } else {
            scan.opaque_code = true;
        }
        return;
    }
    if let Some(code) = inline_code(&command) {
        // A user switch runs its code with the target user's privileges, so
        // the commands inside carry the escalation the switch itself implies.
        let inner_escalation =
            command
                .escalated
                .as_deref()
                .or(if program == "su" { Some("su") } else { None });
        classify_interpreter_input(&program, &code, depth, inner_escalation, scan);
        return;
    }
    let nested = argument_programs(&command);
    scan.commands.push(command);
    for inner in nested {
        record_command(inner, depth, escalation, scan);
    }
}

/// Peel one stage's shell constructs, returning the atomic program text the
/// classifiers should judge. Returns None when the clause contains a construct
/// this unwrapper cannot prove benign, so the caller holds the clause rather
/// than guessing.
pub(crate) fn unwrap_clause(clause: &str) -> Option<String> {
    unwrapper::unwrap_clause(clause)
}

/// Split a normalized command on semicolons and logical operators outside
/// quotes and parentheses, keeping each pipeline together as one clause so
/// downstream pipe stages of an egress command can be checked independently of
/// local pipelines in later statements.
pub(crate) fn split_clauses(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double && depth > 0 => depth -= 1,
            '&' | '|'
                if !in_single
                    && !in_double
                    && depth == 0
                    && i + 1 < chars.len()
                    && chars[i + 1] == c =>
            {
                let trimmed = cur.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                cur.clear();
                i += 2;
                continue;
            }
            ';' if !in_single && !in_double && depth == 0 => {
                let trimmed = cur.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                cur.clear();
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    let trimmed = cur.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

#[cfg(test)]
mod tests;
