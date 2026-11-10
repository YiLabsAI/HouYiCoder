//! Which shell a backend starts, and the command it actually runs.
//!
//! A model writes a command in the spelling of the platform it has in mind.
//! On Windows a POSIX shell answers the command interpreter's null-device
//! spelling by creating a file named nul, while the caller meant to discard
//! the stream. The translation is a lexical scan: a token is rewritten only
//! when it directly follows a redirection operator, outside quotes and
//! heredoc bodies, so the same word as an operand keeps its meaning. A test
//! or arithmetic expression spells a comparison the same way, so it is read
//! as an expression rather than a command and left alone.

pub mod heredoc;
pub mod interpreter;

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

/// Where the scan is reading. Frames nest, so a quoted string inside a
/// substitution inside a quoted string returns to the right place.
#[derive(Clone, Copy)]
enum Frame {
    /// Unquoted command text: quotes, substitutions, heredoc operators, and
    /// redirections start here.
    Command,
    /// Command text inside a command substitution. It ends at the parenthesis
    /// that closes it, and a group inside it opens one more.
    Substitution {
        parens: usize,
    },
    /// Command text inside backticks, which ends at the next unescaped one.
    Backtick,
    SingleQuoted,
    DoubleQuoted,
    /// A conditional test expression, where a comparison is spelled the way a
    /// redirection is and nothing is one.
    Conditional,
    /// An arithmetic expression, where the same comparison spelling appears.
    Arithmetic {
        parens: usize,
    },
    /// A heredoc body: program text, never a command line.
    Body,
}

/// Replace a proven null-device redirect target with the POSIX one. Returns
/// None when no target qualified, so a caller can tell an untouched command
/// from a rewritten one without comparing strings.
fn rewrite_null_redirects(command: &str) -> Option<String> {
    Scan::new(command).run()
}

/// One walk over a command line.
///
/// A command line is a stack of contexts -- quoting, substitutions, test and
/// arithmetic expressions, heredoc bodies -- and a redirect counts only at the
/// command level, so the walk keeps that stack and reads each byte under the
/// context it is currently in. The output buffer stays None until a target
/// actually qualifies, which is what lets the caller tell the two outcomes
/// apart.
struct Scan<'a> {
    command: &'a str,
    text: &'a [u8],
    frames: Vec<Frame>,
    pending: Vec<Heredoc>,
    out: Option<String>,
    copied: usize,
}

impl<'a> Scan<'a> {
    fn new(command: &'a str) -> Self {
        Scan {
            command,
            text: command.as_bytes(),
            frames: vec![Frame::Command],
            pending: Vec::new(),
            out: None,
            copied: 0,
        }
    }

    /// Walk to the end and return the rewritten line, or None when nothing was
    /// replaced.
    fn run(mut self) -> Option<String> {
        let mut i = 0;
        while i < self.text.len() {
            let Some(frame) = self.frames.last().copied() else {
                break;
            };
            i = self.step(frame, i);
        }
        self.out.as_mut()?.push_str(&self.command[self.copied..]);
        self.out
    }

    /// Advance one step under the innermost context.
    fn step(&mut self, frame: Frame, i: usize) -> usize {
        match frame {
            Frame::SingleQuoted => self.single_quoted(i),
            Frame::DoubleQuoted => self.double_quoted(i),
            Frame::Conditional => self.conditional(i),
            Frame::Arithmetic { parens } => self.arithmetic(parens, i),
            Frame::Body => self.body(i),
            Frame::Command | Frame::Substitution { .. } | Frame::Backtick => self.command(frame, i),
        }
    }

    /// A single-quoted string ends only at its closing quote.
    fn single_quoted(&mut self, i: usize) -> usize {
        let text = self.text;
        if text[i] == b'\'' {
            self.frames.pop();
        }
        i + 1
    }

    /// A double-quoted string ends at its closing quote but still nests
    /// substitutions, which run as commands and may redirect.
    fn double_quoted(&mut self, i: usize) -> usize {
        let text = self.text;
        match text[i] {
            b'\\' => (i + 2).min(text.len()),
            b'"' => {
                self.frames.pop();
                i + 1
            }
            b'$' if starts_arithmetic(text, i + 1) => {
                self.frames.push(Frame::Arithmetic { parens: 0 });
                i + 3
            }
            b'$' if text.get(i + 1) == Some(&b'(') => {
                self.frames.push(Frame::Substitution { parens: 0 });
                i + 2
            }
            b'`' => {
                self.frames.push(Frame::Backtick);
                i + 1
            }
            _ => i + 1,
        }
    }

    /// A test expression ends at the closing double bracket; it may contain
    /// quoted text and substitutions but never a redirect.
    fn conditional(&mut self, i: usize) -> usize {
        let text = self.text;
        match text[i] {
            b'\\' => (i + 2).min(text.len()),
            b'\'' => {
                self.frames.push(Frame::SingleQuoted);
                i + 1
            }
            b'"' => {
                self.frames.push(Frame::DoubleQuoted);
                i + 1
            }
            b'$' if starts_arithmetic(text, i + 1) => {
                self.frames.push(Frame::Arithmetic { parens: 0 });
                i + 3
            }
            b'$' if text.get(i + 1) == Some(&b'(') => {
                self.frames.push(Frame::Substitution { parens: 0 });
                i + 2
            }
            b']' if text.get(i + 1) == Some(&b']') => {
                self.frames.pop();
                i + 2
            }
            _ => i + 1,
        }
    }

    /// An arithmetic expression ends at the closing paren that matches the one
    /// that opened it, so the depth is counted.
    fn arithmetic(&mut self, parens: usize, i: usize) -> usize {
        let text = self.text;
        match text[i] {
            b'(' => {
                self.depth(1);
                i + 1
            }
            b')' if parens == 0 => {
                self.frames.pop();
                i + 1
            }
            b')' => {
                self.depth(-1);
                i + 1
            }
            _ => i + 1,
        }
    }

    /// A heredoc body is program text, not a command line, and ends at the line
    /// that matches the delimiter.
    fn body(&mut self, i: usize) -> usize {
        let text = self.text;
        if self.pending.is_empty() {
            self.frames.pop();
            return i;
        }
        let end = line_end(text, i);
        let line = self.command[i..end]
            .strip_suffix('\r')
            .unwrap_or(&self.command[i..end]);
        let heredoc = &self.pending[0];
        let candidate = if heredoc.strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line
        };
        let matches_delimiter = candidate == heredoc.delimiter;
        if matches_delimiter {
            self.pending.remove(0);
        }
        after_line(text, end)
    }

    /// Command context: the only place a redirect is read, and the place every
    /// nested context opens from.
    fn command(&mut self, frame: Frame, i: usize) -> usize {
        let text = self.text;
        match text[i] {
            b'\\' => (i + 2).min(text.len()),
            b'\'' => {
                self.frames.push(Frame::SingleQuoted);
                i + 1
            }
            b'"' => {
                self.frames.push(Frame::DoubleQuoted);
                i + 1
            }
            b'`' if matches!(frame, Frame::Backtick) => {
                self.frames.pop();
                i + 1
            }
            b'`' => {
                self.frames.push(Frame::Backtick);
                i + 1
            }
            b'$' if starts_arithmetic(text, i + 1) => {
                self.frames.push(Frame::Arithmetic { parens: 0 });
                i + 3
            }
            b'$' if text.get(i + 1) == Some(&b'(') => {
                self.frames.push(Frame::Substitution { parens: 0 });
                i + 2
            }
            b'(' if starts_arithmetic(text, i) && starts_word(text, i) => {
                self.frames.push(Frame::Arithmetic { parens: 0 });
                i + 2
            }
            b'(' => {
                self.depth(1);
                i + 1
            }
            b')' => {
                let closes = matches!(self.frames.last(), Some(Frame::Substitution { parens: 0 }));
                if closes {
                    self.frames.pop();
                } else {
                    self.depth(-1);
                }
                i + 1
            }
            b'[' if text.get(i + 1) == Some(&b'[') && starts_word(text, i) => {
                self.frames.push(Frame::Conditional);
                i + 2
            }
            b'#' if starts_word(text, i) => line_end(text, i),
            b'<' if starts_heredoc(text, i) => read_heredoc(self.command, i, &mut self.pending),
            b'\n' if !self.pending.is_empty() => {
                self.frames.push(Frame::Body);
                i + 1
            }
            b'>' => self.redirect(i),
            _ => i + 1,
        }
    }

    /// Move the enclosing group one level deeper or shallower.
    fn depth(&mut self, delta: isize) {
        if let Some(Frame::Substitution { parens } | Frame::Arithmetic { parens }) =
            self.frames.last_mut()
        {
            *parens = parens.saturating_add_signed(delta);
        }
    }

    /// Rewrite the target of the redirect starting here when it is a proven
    /// null device, and return the offset to continue from.
    fn redirect(&mut self, i: usize) -> usize {
        let closes_backtick = matches!(self.frames.last(), Some(Frame::Backtick));
        let (next, target) = redirect_target(self.text, i, closes_backtick);
        let Some((start, end)) = target else {
            return next;
        };
        let buffer = self.out.get_or_insert_with(String::new);
        buffer.push_str(&self.command[self.copied..start]);
        buffer.push_str(POSIX_NULL_DEVICE);
        self.copied = end;
        end
    }
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

/// Whether the opening parenthesis at this offset begins an arithmetic
/// expression, whose comparison operators are not redirections.
fn starts_arithmetic(text: &[u8], at: usize) -> bool {
    text.get(at) == Some(&b'(') && text.get(at + 1) == Some(&b'(')
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

/// The bytes that end a shell word. A backtick is absent on purpose: whether
/// it ends the word depends on the context, and the redirect reader decides
/// that from the frame it is reading under.
fn is_token_boundary(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | b';' | b'|' | b'&' | b'(' | b')' | b'<' | b'>'
    )
}

/// Whether this byte ends a shell word at offset end, counting a backtick only
/// when it closes the backtick substitution the command is inside.
fn ends_word(text: &[u8], end: usize, closes_backtick: bool) -> bool {
    match text.get(end) {
        None => true,
        Some(b'`') => closes_backtick,
        Some(byte) => is_token_boundary(*byte),
    }
}

/// Read an output redirection at this offset. Returns where scanning continues
/// and, when the target is a proven null device, the byte range of that
/// target. A token qualifies only when it is exactly the device name followed
/// by a word boundary: anything that continues the same word is a file name.
///
/// A quoted target that is not the device name is reported as no match with
/// the scan resuming at the quote, because a substitution written inside the
/// quotes still runs a command whose own redirects have to be read.
fn redirect_target(
    text: &[u8],
    operator: usize,
    closes_backtick: bool,
) -> (usize, Option<(usize, usize)>) {
    let mut i = operator + 1;
    if text.get(i) == Some(&b'>') {
        i += 1;
    }
    while matches!(text.get(i), Some(b' ' | b'\t')) {
        i += 1;
    }
    if matches!(text.get(i), Some(b'\'' | b'"')) {
        let quote = text[i];
        return quoted_target(text, i, quote, closes_backtick);
    }
    let end = i + WINDOWS_NULL_DEVICE.len();
    let named =
        text.len() >= end && text[i..end].eq_ignore_ascii_case(WINDOWS_NULL_DEVICE.as_bytes());
    if named && ends_word(text, end, closes_backtick) {
        (end, Some((i, end)))
    } else {
        (i, None)
    }
}

/// Read a redirect target the caller wrote in quotes. The quotes are the
/// shell's syntax rather than part of the name it opens, so a quoted device
/// name is the same device; the whole quoted token is the range replaced.
fn quoted_target(
    text: &[u8],
    start: usize,
    quote: u8,
    closes_backtick: bool,
) -> (usize, Option<(usize, usize)>) {
    let body = start + 1;
    let Some(offset) = text[body..].iter().position(|byte| *byte == quote) else {
        return (text.len(), None);
    };
    let end = body + offset;
    let after = end + 1;
    let named = text[body..end].eq_ignore_ascii_case(WINDOWS_NULL_DEVICE.as_bytes());
    if named && ends_word(text, after, closes_backtick) {
        (after, Some((start, after)))
    } else {
        (start, None)
    }
}

#[cfg(test)]
mod tests;
