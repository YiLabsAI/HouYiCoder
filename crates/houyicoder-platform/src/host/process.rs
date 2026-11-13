//! The native process launcher: the command line a Windows spawn hands the
//! kernel, built under the quoting rules the kernel applies, and the launcher
//! that runs the command inside its job. The FFI that drives the spawn lives
//! in the windows module; the command-line assembly is pure so every quoting
//! edge runs on a host with no Windows kernel.

#[cfg(test)]
mod tests;

#[cfg(target_os = "windows")]
pub use crate::windows::process::{NativeChild, spawn_fenced, spawn_unfenced};

/// Build the command line a Windows CreateProcessW call takes, with each
/// argument quoted and escaped the way the Microsoft C runtime parses it.
///
/// The program is the first token; every argument follows it, separated by a
/// space. The kernel rewrites a quoted program path to the executable it names,
/// so a path with spaces is wrapped rather than rejected. This is the form a
/// child that reads its command line through the C runtime expects -- a posix
/// shell port, a powershell process -- and not the form the command interpreter
/// itself takes, which has its own parser.
pub fn build_command_line(program: &str, args: &[&str]) -> String {
    let mut out = quote_argument(program);
    for arg in args {
        out.push(' ');
        out.push_str(&quote_argument(arg));
    }
    out
}

/// Build the command line for the Windows command interpreter, which does not
/// parse its command line through the C runtime.
///
/// The interpreter path is wrapped in one pair of quotes, and the raw command
/// runs behind the disable-autorun, strip-quotes, and run flags inside a single
/// pair of quotes whose inner quotes are never backslash-escaped -- the
/// interpreter has its own quote parser, and a backslash before a quote would
/// reach the command as a literal backslash rather than disappearing.
pub fn build_cmd_line(cmd_exe: &str, raw_command: &str) -> String {
    format!("\"{cmd_exe}\" /d /s /c \"{raw_command}\"")
}

/// Quote one argument under the Windows command-line rules.
///
/// An argument with no space, tab, newline, or quote passes through unchanged.
/// Any other is wrapped in quotes, with backslashes that precede a quote or the
/// closing quote doubled, and quotes escaped. This is the parsing the C runtime
/// applies, so a child that reads its command line through it receives the
/// bytes the caller meant.
fn quote_argument(arg: &str) -> String {
    let needs_quotes = arg.is_empty()
        || arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '"'));
    if !needs_quotes {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        if c == '"' {
            // A backslash run before a quote doubles, and the quote is escaped.
            push_run(&mut out, backslashes * 2);
            out.push_str("\\\"");
        } else {
            push_run(&mut out, backslashes);
            out.push(c);
        }
        backslashes = 0;
    }
    // A trailing backslash run before the closing quote doubles.
    push_run(&mut out, backslashes * 2);
    out.push('"');
    out
}

/// Append a run of backslashes without reallocating per character.
fn push_run(out: &mut String, count: usize) {
    if count > 0 {
        out.reserve(count);
        for _ in 0..count {
            out.push('\\');
        }
    }
}
