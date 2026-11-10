//! Who reads a stream, and what text that reader receives.
//!
//! A heredoc body is text to the shell and code to the program reading it, so
//! the body is judged by that reader rather than as part of the command line.
//! The same question decides a pipeline stage: an interpreter that is not the
//! first runs what arrives on its standard input, which is code only when the
//! stage before it writes that text literally. A reader this scan cannot
//! identify, or text a stage rewrites first, leaves the code unknown.

use houyicoder_api::shell::heredoc::{heredoc_starts, strip_heredoc_bodies, walk_heredoc_bodies};

use crate::compound::split_compound;

use super::MAX_DEPTH;
use super::ShellCommand;
use super::ShellScan;
use super::Stage;
use super::classify_interpreter_input;
use super::code_reader;
use super::inline_code;
use super::shell_interpreter;
use super::split_clauses;
use super::stage_program;

/// One heredoc body and the program that reads it.
pub(super) struct ProgramBody {
    program: String,
    /// Whether the program takes its code from standard input, in which case
    /// the body is the code that runs.
    reads_stdin: bool,
    /// Whether a program downstream reads the body as code but a stage
    /// between them transforms it, so the text here is not the code that runs.
    unreadable: bool,
    /// Whether the shell passes the body as literal text.
    quoted: bool,
    /// The privilege the command runs under, when a launcher grants one.
    escalation: Option<String>,
    body: String,
}

/// The heredoc bodies of a command, each paired with the program that reads
/// it.
pub(super) fn program_bodies(content: &str) -> Vec<ProgramBody> {
    let mut bodies: Vec<ProgramBody> = Vec::new();
    let mut group = 0;
    walk_heredoc_bodies(
        content,
        false,
        |operator_line, ordinal, quoted, body_line| {
            if ordinal == 0 || group == bodies.len() {
                group = bodies.len();
                let mut readers = heredoc_readers(operator_line);
                if readers.is_empty() {
                    // The operator line names no program this scan can read; the
                    // body still holds text, so it keeps an unnamed entry.
                    readers.push(HeredocReader::unnamed(quoted));
                }
                bodies.extend(readers.into_iter().map(|reader| ProgramBody {
                    program: reader.program,
                    reads_stdin: reader.reads_stdin,
                    unreadable: reader.unreadable,
                    quoted: reader.quoted,
                    escalation: reader.escalation,
                    body: String::new(),
                }));
            }
            let last = bodies.len() - group - 1;
            let target = group + ordinal.min(last);
            bodies[target].body.push_str(body_line);
            bodies[target].body.push('\n');
        },
    );
    bodies
}

/// Judge one heredoc body by who reads it.
pub(super) fn scan_body(body: ProgramBody, scan: &mut ShellScan) {
    if body.unreadable {
        // A program downstream reads the body as code, but a stage between
        // them transforms it, so the text here is not the code that runs.
        scan.opaque_code = true;
        return;
    }
    // A shell reader parses the body whether or not the shell expanded it, so
    // its lines are commands either way.
    if body.reads_stdin && shell_interpreter(&body.program) {
        classify_interpreter_input(
            &body.program,
            &body.body,
            MAX_DEPTH,
            body.escalation.as_deref(),
            scan,
        );
        return;
    }
    if !body.quoted {
        // The shell runs the substitutions an unquoted body carries before any
        // program reads the text; the top-level scan walks them. Another
        // interpreter would read the shell's expansion rather than this text,
        // and an expansion this scan does not evaluate could change the code
        // it receives.
        if body.reads_stdin && names_expansion(&body.body) {
            scan.opaque_code = true;
            return;
        }
    }
    if body.reads_stdin {
        classify_interpreter_input(
            &body.program,
            &body.body,
            MAX_DEPTH,
            body.escalation.as_deref(),
            scan,
        );
    }
}

/// Whether shell text carries an expansion this scan does not evaluate.
fn names_expansion(text: &str) -> bool {
    text.contains('$') || text.contains('`')
}

/// The program that reads one heredoc body on an operator line.
struct HeredocReader {
    program: String,
    reads_stdin: bool,
    unreadable: bool,
    quoted: bool,
    escalation: Option<String>,
}

impl HeredocReader {
    /// An entry for a body whose operator line names no program.
    fn unnamed(quoted: bool) -> Self {
        Self {
            program: String::new(),
            reads_stdin: false,
            unreadable: false,
            quoted,
            escalation: None,
        }
    }
}

/// The reader of each heredoc on an operator line, in the order the heredocs
/// appear. A body belongs to the program of the stage carrying its operator,
/// unless that stage passes its input through unchanged to a program
/// downstream that reads code from standard input.
fn heredoc_readers(operator_line: &str) -> Vec<HeredocReader> {
    let stages = split_compound(operator_line);
    let mut readers = Vec::new();
    for (at, stage) in stages.iter().enumerate() {
        let starts = heredoc_starts(stage);
        if starts.is_empty() {
            continue;
        }
        let mut program = stage_program_name(stage);
        let mut escalation = stage_escalation(stage);
        let mut reads_stdin = stage_reader_program(stage).is_some();
        let mut unreadable = false;
        if !reads_stdin && runs_cat(stage) {
            let passes = is_pass_through_cat(stage);
            match downstream_reader(&stages[at + 1..]) {
                StdinReader::Code {
                    program: reader,
                    escalation: reader_escalation,
                } if passes => {
                    program = reader;
                    escalation = reader_escalation;
                    reads_stdin = true;
                }
                // The text a downstream interpreter runs includes what this
                // stage wrote besides the body, or the stage rewrote the body,
                // so the body here is not the code that runs.
                StdinReader::Code { .. } | StdinReader::Obscured => {
                    reads_stdin = true;
                    unreadable = true;
                }
                StdinReader::None => {}
            }
        }
        for start in starts {
            readers.push(HeredocReader {
                program: program.clone(),
                reads_stdin,
                unreadable,
                quoted: start.quoted,
                escalation: escalation.clone(),
            });
        }
    }
    readers
}

/// The program a stage runs, or an empty name when it names none.
fn stage_program_name(segment: &str) -> String {
    match stage_program(segment) {
        Stage::Program { command, .. } => command.program().to_string(),
        Stage::Empty => String::new(),
    }
}

/// The privilege a stage runs under, when a launcher grants one.
fn stage_escalation(segment: &str) -> Option<String> {
    match stage_program(segment) {
        Stage::Program { command, .. } => command.escalated.clone(),
        Stage::Empty => None,
    }
}

/// Whether the stage runs the file copier, which forwards its standard input
/// when it is reading from it.
fn runs_cat(segment: &str) -> bool {
    matches!(stage_program(segment), Stage::Program { command, .. } if command.runs("cat"))
}

/// Whether the stage hands its standard input to the next stage unchanged. An
/// option rewrites the text in some way, and a file operand puts other text on
/// the stream ahead of it, so either one means the body is not what the next
/// stage receives.
fn is_pass_through_cat(segment: &str) -> bool {
    let Stage::Program { command, .. } = stage_program(segment) else {
        return false;
    };
    command.runs("cat")
        && command
            .args()
            .iter()
            .all(|word| word == "-" || word == "--" || word.starts_with("<<"))
}

/// A stage that runs code arriving on its standard input.
enum StdinReader {
    /// No later stage reads code from the stream.
    None,
    /// The next program to read the stream is this one, and every stage in
    /// between passes the text through unchanged.
    Code {
        program: String,
        escalation: Option<String>,
    },
    /// A program reads the stream as code, but a stage between them rewrites
    /// the text first.
    Obscured,
}

/// The program downstream of a pass-through stage that reads code from the
/// stream, if any.
fn downstream_reader(stages: &[String]) -> StdinReader {
    let mut transparent = true;
    for stage in stages {
        if is_pass_through_cat(stage) {
            continue;
        }
        if let Some(program) = stage_reader_program(stage) {
            return if transparent {
                StdinReader::Code {
                    program,
                    escalation: stage_escalation(stage),
                }
            } else {
                StdinReader::Obscured
            };
        }
        transparent = false;
    }
    StdinReader::None
}

/// The program a stage starts that reads its code from standard input.
fn stage_reader_program(segment: &str) -> Option<String> {
    let Stage::Program { command, .. } = stage_program(segment) else {
        return None;
    };
    interpreter_reads_stdin(&command).then(|| command.program().to_string())
}

/// Whether an interpreter takes its code from the stream rather than from an
/// inline flag or a code file.
fn interpreter_reads_stdin(command: &ShellCommand) -> bool {
    if code_reader(command.program()).is_none() {
        return false;
    }
    if inline_code(command).is_some() {
        return false;
    }
    // A word that is neither an option nor a heredoc operator is a code file
    // or module path, which the interpreter reads instead of the stream.
    command
        .args()
        .iter()
        .all(|word| word.starts_with('-') || word.starts_with("<<"))
}

/// Judge the code a later pipeline stage reads from its standard input.
///
/// An interpreter that is not the first stage and has no inline code or script
/// file runs what arrives on its standard input. When the stage before it
/// writes that text literally, the text is code this scan can read; otherwise
/// the code is unknown and the caller holds.
pub(super) fn scan_pipelines(content: &str, depth: u8, scan: &mut ShellScan) {
    let text = strip_heredoc_bodies(content);
    for clause in split_clauses(&text) {
        let stages = pipeline_stages(&clause);
        for (at, stage) in stages.iter().enumerate().skip(1) {
            let Some(program) = stage_reader_program(stage) else {
                continue;
            };
            // A body's own pairing already judged this input.
            if stage.contains("<<") {
                continue;
            }
            let escalation = stage_escalation(stage);
            match literal_stdin_source(&stages[at - 1]) {
                StdinSource::Code(code) => {
                    classify_interpreter_input(&program, &code, depth, escalation.as_deref(), scan);
                }
                StdinSource::Handled => {}
                StdinSource::Unknown => scan.opaque_code = true,
            }
        }
    }
}

/// The text an upstream stage writes to its standard output.
enum StdinSource {
    /// The stage writes literal text this scan can read.
    Code(String),
    /// The stage writes nothing, or a heredoc reader already judged its text.
    Handled,
    /// The stage writes text this scan cannot resolve.
    Unknown,
}

/// The literal text a stage writes, when it is an echo or printf call whose
/// words are exactly what it prints.
fn literal_stdin_source(upstream: &str) -> StdinSource {
    if upstream.contains("<<") {
        return StdinSource::Handled;
    }
    let Stage::Program { command, exact } = stage_program(upstream) else {
        return StdinSource::Unknown;
    };
    if !exact {
        return StdinSource::Unknown;
    }
    let text = if command.runs("echo") {
        echo_text(&command)
    } else if command.runs("printf") {
        printf_text(&command)
    } else {
        return StdinSource::Unknown;
    };
    match text {
        Some(code) if code.is_empty() => StdinSource::Handled,
        Some(code) => StdinSource::Code(code),
        None => StdinSource::Unknown,
    }
}

/// The text echo prints, when its words are what it prints.
///
/// An option changes what echo writes, and an expansion makes the output
/// something this scan has not read, so either one leaves the text unknown.
fn echo_text(command: &ShellCommand) -> Option<String> {
    if command
        .args()
        .iter()
        .any(|word| word.starts_with('-') || names_expansion(word))
    {
        return None;
    }
    Some(command.args().join(" "))
}

/// The text printf prints, when its format carries no conversion or escape.
/// A format string with either one writes something other than its words, so
/// the text it emits is not this scan's to read.
fn printf_text(command: &ShellCommand) -> Option<String> {
    let args = command.args();
    let (format, rest) = args.split_first()?;
    if format.contains('%') || format.contains('\\') || !rest.is_empty() {
        return None;
    }
    Some(format.clone())
}

/// Split a clause on its pipeline operators, keeping each stage as written.
fn pipeline_stages(clause: &str) -> Vec<String> {
    let chars: Vec<char> = clause.chars().collect();
    let mut stages = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            '\\' if !in_single && i + 1 < chars.len() => {
                current.push(c);
                current.push(chars[i + 1]);
                i += 2;
                continue;
            }
            '(' if !in_single && !in_double => {
                depth += 1;
                current.push(c);
            }
            ')' if !in_single && !in_double && depth > 0 => {
                depth -= 1;
                current.push(c);
            }
            '|' if !in_single
                && !in_double
                && depth == 0
                && chars.get(i + 1) != Some(&'|')
                && chars.get(i.wrapping_sub(1)) != Some(&'|') =>
            {
                stages.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(c),
        }
        i += 1;
    }
    stages.push(current.trim().to_string());
    stages.retain(|stage| !stage.is_empty());
    stages
}
