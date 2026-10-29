//! Which shell a backend starts, and the command it actually runs.
//!
//! A model writes a command in the spelling of the platform it has in mind.
//! On Windows a POSIX shell answers the command interpreter's null-device
//! spelling by creating a file named nul, while the caller meant to discard
//! the stream. The translation is a lexical scan: a token is rewritten only
//! when it directly follows a redirection operator, outside quotes and
//! heredoc bodies, so the same word as an operand keeps its meaning.

/// The shell a sandbox backend starts for one command.
///
/// The value is a property of the backend, not of the caller: a backend
/// reports the shell it will actually start, and the command translation reads
/// that answer. A caller cannot ask for a shell the host does not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// A POSIX shell (bash or sh), on every unix host.
    Posix,
    /// The bash that ships with Git for Windows: a POSIX shell, so its null
    /// device is the POSIX one rather than the interpreter's device name.
    GitBash,
    /// The Windows command interpreter, whose redirect targets are Windows
    /// paths and device names.
    Cmd,
}

/// A command in the spelling the caller wrote and in the spelling the host
/// will run.
///
/// Both are kept because they answer different questions. The original is what
/// the model proposed and what the transcript records; the actual is what the
/// approval describes and what the shell receives. They differ only when the
/// host's shell reads a token differently from the platform the command was
/// written for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommand {
    original: String,
    actual: String,
}

impl ShellCommand {
    /// The host's form of a command written for the given shell. Every shell
    /// but Git Bash runs the text unchanged.
    pub fn for_shell(command: &str, shell: Shell) -> Self {
        let actual = match shell {
            Shell::GitBash => rewrite_null_redirects(command).unwrap_or_else(|| command.to_owned()),
            Shell::Posix | Shell::Cmd => command.to_owned(),
        };
        Self {
            original: command.to_owned(),
            actual,
        }
    }

    /// Whether the host will run a different text than the caller wrote.
    pub fn changed(&self) -> bool {
        self.original != self.actual
    }

    /// The text the host runs, handed over.
    pub fn into_actual(self) -> String {
        self.actual
    }
}

/// The null device as a POSIX shell spells it.
const POSIX_NULL_DEVICE: &str = "/dev/null";

/// The null device as the Windows command interpreter spells it.
const WINDOWS_NULL_DEVICE: &str = "nul";

/// A heredoc whose body starts on the next line.
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

/// Where the scan is reading.
enum Mode {
    /// Unquoted command text: quotes, heredoc operators, and redirections
    /// start here.
    Plain,
    SingleQuoted,
    DoubleQuoted,
    /// A heredoc body: program text, never a command line.
    Body,
}

/// Replace a proven null-device redirect target with the POSIX one. Returns
/// None when no target qualified, so a caller can tell an untouched command
/// from a rewritten one without comparing strings.
fn rewrite_null_redirects(command: &str) -> Option<String> {
    let text = command.as_bytes();
    let mut pending: Vec<Heredoc> = Vec::new();
    let mut mode = Mode::Plain;
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;
    while i < text.len() {
        match mode {
            Mode::SingleQuoted => {
                if text[i] == b'\'' {
                    mode = Mode::Plain;
                }
                i += 1;
            }
            Mode::DoubleQuoted => match text[i] {
                b'\\' => i = (i + 2).min(text.len()),
                b'"' => {
                    mode = Mode::Plain;
                    i += 1;
                }
                _ => i += 1,
            },
            Mode::Body => {
                if pending.is_empty() {
                    mode = Mode::Plain;
                    continue;
                }
                let end = line_end(text, i);
                let line = command[i..end]
                    .strip_suffix('\r')
                    .unwrap_or(&command[i..end]);
                let heredoc = pending.first().expect("checked above");
                let candidate = if heredoc.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line
                };
                if candidate == heredoc.delimiter {
                    pending.remove(0);
                }
                i = after_line(text, end);
            }
            Mode::Plain => match text[i] {
                b'\\' => i = (i + 2).min(text.len()),
                b'\'' => {
                    mode = Mode::SingleQuoted;
                    i += 1;
                }
                b'"' => {
                    mode = Mode::DoubleQuoted;
                    i += 1;
                }
                b'#' if starts_word(text, i) => i = line_end(text, i),
                b'<' if starts_heredoc(text, i) => {
                    i = read_heredoc(command, i, &mut pending);
                }
                b'\n' if !pending.is_empty() => {
                    mode = Mode::Body;
                    i += 1;
                }
                b'>' => {
                    let (next, target) = redirect_target(text, i);
                    match target {
                        Some((start, end)) => {
                            let buffer = out.get_or_insert_with(String::new);
                            buffer.push_str(&command[copied..start]);
                            buffer.push_str(POSIX_NULL_DEVICE);
                            copied = end;
                            i = end;
                        }
                        None => i = next,
                    }
                }
                _ => i += 1,
            },
        }
    }
    out.as_mut()?.push_str(&command[copied..]);
    out
}

/// The byte offset of the newline ending the line that starts at offset.
fn line_end(text: &[u8], start: usize) -> usize {
    text[start..]
        .iter()
        .position(|b| *b == b'\n')
        .map(|offset| start + offset)
        .unwrap_or(text.len())
}

/// The offset of the first byte after the line ending at end.
fn after_line(text: &[u8], end: usize) -> usize {
    if end < text.len() { end + 1 } else { end }
}

/// Whether a hash at this offset begins a comment, which shell grammar makes
/// true at the start of a word and false inside one.
fn starts_word(text: &[u8], at: usize) -> bool {
    match at.checked_sub(1).map(|prev| text[prev]) {
        None => true,
        Some(prev) => matches!(
            prev,
            b' ' | b'\t' | b'\n' | b'\r' | b';' | b'|' | b'&' | b'('
        ),
    }
}

/// Whether the angle bracket at this offset opens a heredoc. A triple angle
/// bracket is a here string, and a single one is an input redirection.
fn starts_heredoc(text: &[u8], at: usize) -> bool {
    text.get(at + 1) == Some(&b'<') && text.get(at + 2) != Some(&b'<')
}

/// Read a heredoc operator and its delimiter, recording the body that starts
/// on the next line. Returns the offset of the first byte after the delimiter,
/// where the same command line continues.
fn read_heredoc(command: &str, operator: usize, pending: &mut Vec<Heredoc>) -> usize {
    let text = command.as_bytes();
    let mut i = operator + 2;
    let strip_tabs = text.get(i) == Some(&b'-');
    if strip_tabs {
        i += 1;
    }
    while matches!(text.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    match text.get(i) {
        Some(&quote @ (b'\'' | b'"')) => {
            let body_start = i + 1;
            let Some(offset) = text[body_start..].iter().position(|b| *b == quote) else {
                return text.len();
            };
            let end = body_start + offset;
            pending.push(Heredoc {
                delimiter: command[body_start..end].to_owned(),
                strip_tabs,
            });
            end + 1
        }
        Some(_) => {
            let end = text[i..]
                .iter()
                .position(|b| is_token_boundary(*b))
                .map(|offset| i + offset)
                .unwrap_or(text.len());
            pending.push(Heredoc {
                delimiter: command[i..end].to_owned(),
                strip_tabs,
            });
            end
        }
        None => i,
    }
}

/// The bytes that end a shell word.
fn is_token_boundary(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | b';' | b'|' | b'&' | b'(' | b')' | b'<' | b'>'
    )
}

/// Read an output redirection at this offset. Returns where scanning continues
/// and, when the target is a proven null device, the byte range of that
/// target. A token qualifies only when it is exactly the device name followed
/// by a word boundary: anything that continues the same word is a file name.
fn redirect_target(text: &[u8], operator: usize) -> (usize, Option<(usize, usize)>) {
    let mut i = operator + 1;
    if text.get(i) == Some(&b'>') {
        i += 1;
    }
    while matches!(text.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    let end = i + WINDOWS_NULL_DEVICE.len();
    let named =
        text.len() >= end && text[i..end].eq_ignore_ascii_case(WINDOWS_NULL_DEVICE.as_bytes());
    let bounded = match text.get(end) {
        None => true,
        Some(byte) => is_token_boundary(*byte),
    };
    if named && bounded {
        (end, Some((i, end)))
    } else {
        (i, None)
    }
}

#[cfg(test)]
mod tests;
