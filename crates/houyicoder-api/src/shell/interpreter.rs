//! Reading the code an interpreter is handed.
//!
//! A program that only reads is the one interpreter use the approval can
//! vouch for: it consumes a response or a local file and prints part of it.
//! The test is positive on purpose. A module, an attribute, or a call this
//! file has not named is refused, so a spelling the author did not think of
//! fails closed. The same rule decides which interpreter code a command walk
//! may set aside, because that code carries the separators the walk splits on.

use crate::sandbox::{names_credential_path, protected_path_hits};

use super::heredoc::heredoc_starts;

/// The modules a read-only program may import: data shapes, text handling,
/// number handling, file finding, and the environment.
///
/// The environment module is here because a read-only parser reports the
/// configuration and the directory tree it read. The attribute check below
/// limits what it may be read through, so a call that would start a program or
/// remove a file through the same module is refused.
const READ_ONLY_MODULES: &[&str] = &[
    "base64",
    "collections",
    "csv",
    "datetime",
    "decimal",
    "difflib",
    "fractions",
    "functools",
    "glob",
    "hashlib",
    "html",
    "itertools",
    "json",
    "math",
    "os",
    "re",
    "statistics",
    "string",
    "sys",
    "textwrap",
    "time",
    "typing",
    "unicodedata",
];

/// Names a read-only program never needs. Each one is a way out: reaching the
/// network, starting a program, rebuilding a name the import list cannot see,
/// or reaching a frame. The scan is by whole word, so a longer identifier that
/// merely contains one is not a match.
const READ_ONLY_REFUSED: &[&str] = &[
    "__bases__",
    "__class__",
    "__code__",
    "__dict__",
    "__getattribute__",
    "__getattr__",
    "__loader__",
    "__mro__",
    "__reduce__",
    "_getframe",
    "f_builtins",
    "f_globals",
    "func_globals",
    "load_module",
    "meta_path",
    "modules",
    "attrgetter",
    "methodcaller",
    "__builtins__",
    "__globals__",
    "__import__",
    "__subclasses__",
    "aiohttp",
    "breakpoint",
    "compile",
    "ctypes",
    "delattr",
    "eval",
    "exec",
    "ftplib",
    "getattr",
    "globals",
    "http",
    "httpx",
    "importlib",
    "input",
    "locals",
    "pathlib",
    "popen",
    "pty",
    "requests",
    "setattr",
    "shutil",
    "smtplib",
    "socket",
    "spawn",
    "subprocess",
    "system",
    "telnetlib",
    "tempfile",
    "urllib",
    "vars",
];

/// The attributes of the standard-input module a read-only program may read.
///
/// The module itself is permitted because reading the response through it is
/// the idiom, but its other attributes are not: the frame accessor and the
/// module table are two ways back to every name the import list refused, and
/// reading a response never needs either.
const READ_ONLY_SYS_ATTRS: &[&str] = &[
    "argv",
    "maxsize",
    "stderr",
    "stdin",
    "stdout",
    "version",
    "version_info",
];

/// The attributes of the environment module a read-only program may read.
/// Each one inspects a name, a path, or a directory entry. Every other name on
/// the module starts a program, removes a file, or changes the working
/// directory, so nothing outside this list is admitted.
const READ_ONLY_OS_ATTRS: &[&str] = &[
    "cpu_count",
    "environ",
    "fspath",
    "getcwd",
    "getenv",
    "getpid",
    "linesep",
    "listdir",
    "lstat",
    "name",
    "path",
    "scandir",
    "sep",
    "stat",
    "uname",
    "walk",
];

/// The mode strings a read-only open may carry. An update mode opens the file
/// for writing as well, so it is not on the list however harmless it looks.
const READ_MODES: &[&str] = &["r", "rb", "rt"];

/// Whether interpreter code only reads what it is given.
///
/// Every import names a permitted module, no refused name appears, and each
/// call that reaches outside the process either inspects a name or opens a
/// file for reading. Code with no import at all passes the import half, which
/// is what a short one-liner looks like.
pub fn is_read_only_program(code: &str) -> bool {
    // A credential or a protected file is refused before the string literals
    // are blanked. The path the code reaches sits inside a literal, so the
    // scans that guard it read the command line, and a heredoc hands the body
    // to the program without the path ever appearing there.
    if names_credential_path(code) || protected_path_hits(code).any() {
        return false;
    }
    let lower = code.to_ascii_lowercase();
    let body = without_imports(&lower);
    // A refused name that appears only inside a string literal is data: a
    // dictionary key or a printed label, not a primitive the code calls.
    let code_only = without_string_literals(&body);
    if READ_ONLY_REFUSED
        .iter()
        .any(|name| names_word(&code_only, name))
    {
        return false;
    }
    imports_are_permitted(&lower)
        && attributes_are_permitted(&code_only, "sys", READ_ONLY_SYS_ATTRS)
        && attributes_are_permitted(&code_only, "os", READ_ONLY_OS_ATTRS)
        // The open scan reads the text with its literals kept, so a literal
        // that spells a call is refused rather than assumed to be data.
        && open_calls_are_read_only(&body)
}

/// The command with every verified read-only interpreter body replaced by a
/// no-op, or None when one body is not read-only.
///
/// A body is removed rather than skipped because it contains the separators a
/// caller splits the command on: a JSON one-liner carries semicolons and
/// parentheses, and leaving them in would cut the script into segments the
/// caller reads as commands. A body arrives as an inline -c or -e argument, or
/// through a quoted heredoc on standard input. An unquoted heredoc refuses the
/// whole command, because the shell expands that body before the program reads
/// it, so the text the program runs is not the text this rule judged.
pub fn without_read_only_code(command: &str) -> Option<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].is_ascii_alphabetic() {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && (chars[index].is_ascii_alphanumeric() || chars[index] == '_') {
            index += 1;
        }
        let word: String = chars[start..index].iter().collect();
        if !is_interpreter(&word) {
            out.push_str(&word);
            continue;
        }
        if let Some((body, after)) = inline_body(&chars, index) {
            if !is_read_only_program(&body) {
                return None;
            }
            out.push_str(" true ");
            index = after;
            continue;
        }
        match heredoc_code(&chars, index) {
            HeredocCode::Absent => out.push_str(&word),
            HeredocCode::Unverified => return None,
            HeredocCode::Verified { body, after } => {
                if !is_read_only_program(&body) {
                    return None;
                }
                out.push_str(" true ");
                index = after;
            }
        }
    }
    Some(out)
}

/// Whether a word names an interpreter whose code this rule can read. Only
/// Python: the check reads import statements, and an interpreter without them
/// has no form to read.
fn is_interpreter(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        "python" | "python2" | "python3"
    )
}

/// The code an interpreter receives through a heredoc after this word.
enum HeredocCode {
    /// No heredoc operator follows the word.
    Absent,
    /// A heredoc operator follows the word, and this rule cannot vouch for the
    /// body: several operators on the line, an unquoted delimiter, or a body
    /// whose closing delimiter never appears.
    Unverified,
    /// A quoted heredoc body and the index just past its closing delimiter
    /// line.
    Verified { body: String, after: usize },
}

/// Read the heredoc an interpreter word is handed on standard input. The body
/// starts on the line after the operator and ends at the delimiter line.
fn heredoc_code(chars: &[char], from: usize) -> HeredocCode {
    let line_end = chars[from..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |at| from + at);
    let operator_line: String = chars[from..line_end].iter().collect();
    let starts = heredoc_starts(&operator_line);
    let [start] = starts.as_slice() else {
        return if starts.is_empty() {
            HeredocCode::Absent
        } else {
            HeredocCode::Unverified
        };
    };
    if !start.quoted {
        return HeredocCode::Unverified;
    }
    let mut body = String::new();
    let mut at = line_end + 1;
    while at < chars.len() {
        let end = chars[at..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |offset| at + offset);
        let line: String = chars[at..end].iter().collect();
        let line = line.strip_suffix('\r').unwrap_or(&line);
        let compared = if start.tab_strip {
            line.trim_start_matches('\t')
        } else {
            line
        };
        if compared == start.delim {
            let after = if end < chars.len() { end + 1 } else { end };
            return HeredocCode::Verified { body, after };
        }
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
        at = end + 1;
    }
    HeredocCode::Unverified
}

/// The quoted body after a -c or -e flag, and the index just past its quote.
fn inline_body(chars: &[char], from: usize) -> Option<(String, usize)> {
    let mut index = from;
    while chars.get(index) == Some(&' ') {
        index += 1;
    }
    if chars.get(index) != Some(&'-') {
        return None;
    }
    if !matches!(
        chars.get(index + 1).map(|c| c.to_ascii_lowercase()),
        Some('c' | 'e')
    ) {
        return None;
    }
    index += 2;
    while chars.get(index) == Some(&' ') {
        index += 1;
    }
    let quote = *chars.get(index)?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let body_start = index + 1;
    let mut end = body_start;
    // A double-quoted body escapes its quote with a backslash, so a backslash
    // skips the character after it: without this an escaped quote inside a
    // Python format string ends the body early, and the tail reads as an
    // unknown command in the outer walk.
    while end < chars.len() {
        if quote == '"' && chars[end] == '\\' && end + 1 < chars.len() {
            end += 2;
            continue;
        }
        if chars[end] == quote {
            break;
        }
        end += 1;
    }
    let body: String = chars[body_start..end].iter().collect();
    Some((body, end + 1))
}

/// The code with every import statement replaced by a space.
///
/// The statement names a module rather than reaching into it, so the attribute
/// checks below must not read it as an attribute read: an environment import
/// would fail the rule that admits only the lookup table off the module.
fn without_imports(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    for statement in code.split_inclusive([';', '\n']) {
        let trimmed = statement.trim_start();
        if trimmed.starts_with("import ") || trimmed.starts_with("from ") {
            out.push(' ');
        } else {
            out.push_str(statement);
        }
    }
    out
}

/// The code with every string literal and comment replaced by a space.
///
/// A name inside a quoted run or a comment is data the program handles, not a
/// primitive it calls, so the refused-name scan reads the code around those
/// runs. The literals are removed with their contents so two of them cannot
/// fuse into a name neither contains.
fn without_string_literals(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push(' ');
            continue;
        }
        if c != '\'' && c != '"' {
            out.push(c);
            i += 1;
            continue;
        }
        i += 1;
        while i < chars.len() {
            if chars[i] == '\\' && i + 1 < chars.len() {
                i += 2;
                continue;
            }
            if chars[i] == c {
                i += 1;
                break;
            }
            i += 1;
        }
        out.push(' ');
    }
    out
}

/// Whether every import statement in the code names a permitted module.
///
/// Statements are read one at a time because an import is the only construct
/// that brings a capability in, and the first module of each name is what
/// decides: a submodule is a part of the module that was permitted. The
/// environment module is the exception. A plain import of it stays under the
/// dotted attribute rule only while it binds that one name, and a from import
/// brings a name in bare, where the attribute rule cannot see it, so only the
/// lookup table may be named there.
fn imports_are_permitted(code: &str) -> bool {
    for statement in code.split([';', '\n']) {
        let statement = statement.trim();
        if let Some(names) = statement.strip_prefix("import ")
            && !plain_imports_are_permitted(names)
        {
            return false;
        }
        if let Some(rest) = statement.strip_prefix("from ")
            && !from_import_is_permitted(rest)
        {
            return false;
        }
    }
    true
}

/// Whether the names a plain import binds are all permitted.
fn plain_imports_are_permitted(names: &str) -> bool {
    for name in names.split(',') {
        let name = name.trim();
        let path = name.split_whitespace().next().unwrap_or("");
        let top = path.split('.').next().unwrap_or("");
        if !READ_ONLY_MODULES.contains(&top) {
            return false;
        }
        // The environment module's capability check is keyed to the dotted
        // spelling this form leaves intact, so a renamed binding would escape
        // it. The path submodule is the one dotted form admitted, because it
        // binds the same name and holds only string and stat work.
        if top == "os" && name != "os" && name != "os.path" {
            return false;
        }
    }
    true
}

/// Whether a from import names a permitted module and a permitted name.
///
/// The imported names arrive without the module prefix the attribute rule
/// matches, so the environment module is limited here to the attributes that
/// list admits. The path submodule is admitted whole: it holds only string
/// and stat work. An alias is still the name it was bound to, so the bound
/// name is read before its alias.
fn from_import_is_permitted(rest: &str) -> bool {
    let Some((module, names)) = rest.split_once(" import ") else {
        return false;
    };
    let module = module.trim();
    let top = module.split('.').next().unwrap_or("");
    if !READ_ONLY_MODULES.contains(&top) {
        return false;
    }
    if top != "os" {
        return true;
    }
    if module == "os.path" {
        return true;
    }
    if module != "os" {
        return false;
    }
    names.split(',').all(|name| {
        let bound = name.split_whitespace().next().unwrap_or("");
        READ_ONLY_OS_ATTRS.contains(&bound)
    })
}

/// Whether the text contains the name as a whole word.
fn names_word(haystack: &str, name: &str) -> bool {
    find_word(haystack, name, 0).is_some()
}

/// The byte offset of the next whole-word occurrence at or after from.
fn find_word(haystack: &str, name: &str, from: usize) -> Option<usize> {
    let mut cursor = from;
    while let Some(at) = haystack[cursor..].find(name) {
        let start = cursor + at;
        let end = start + name.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        let opens = before.is_none_or(|c| !is_name_char(c));
        let closes = after.is_none_or(|c| !is_name_char(c));
        if opens && closes {
            return Some(start);
        }
        cursor = start + 1;
    }
    None
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether every reference to one module is an attribute read a read-only
/// program needs.
///
/// The module is permitted; its bare name and the attributes not on the
/// permitted list are not. The standard-input module reaches the frames of
/// every function on the stack, the environment module reaches every process
/// and file call the import list refused, and a bare reference binds the
/// module to another name the dotted rule never sees. Whitespace around the
/// dot is Python's, so a spaced attribute read is judged like the compact one
/// rather than slipping past it.
fn attributes_are_permitted(code: &str, module: &str, permitted: &[&str]) -> bool {
    let mut from = 0;
    while let Some(at) = find_word(code, module, from) {
        let mut cursor = at + module.len();
        let skipped = code[cursor..].trim_start();
        cursor += code[cursor..].len() - skipped.len();
        let Some(after_dot) = skipped.strip_prefix('.') else {
            return false;
        };
        cursor += 1;
        let skipped = after_dot.trim_start();
        cursor += after_dot.len() - skipped.len();
        let end = skipped
            .find(|c: char| !is_name_char(c))
            .unwrap_or(skipped.len());
        if !permitted.contains(&&skipped[..end]) {
            return false;
        }
        from = cursor + end;
    }
    true
}

/// What an open call says about its mode.
enum OpenMode {
    /// The call names no mode, which reads by default.
    Absent,
    /// The call names a literal mode.
    Literal(String),
    /// The call names a mode the scan cannot read.
    Opaque,
}

/// Whether every open call opens for reading.
///
/// The path may be a temporary file or a fetched response; the mode is what
/// decides whether the call writes, so the mode is what this reads. A mode
/// spelled as a variable or a computed string says nothing, so it is refused.
/// A bare reference to the callable is refused as well: assigning it to a name
/// and calling that name later would carry any mode the scan never saw.
fn open_calls_are_read_only(code: &str) -> bool {
    let mut from = 0;
    while let Some(at) = find_word(code, "open", from) {
        let after = at + "open".len();
        from = after;
        let Some(args) = code[after..].trim_start().strip_prefix('(') else {
            return false;
        };
        let Some(list) = read_call_args(args) else {
            return false;
        };
        match open_mode(list) {
            OpenMode::Absent => {}
            OpenMode::Literal(mode) if READ_MODES.contains(&mode.as_str()) => {}
            OpenMode::Literal(_) | OpenMode::Opaque => return false,
        }
    }
    true
}

/// The text between an opening parenthesis and its match, or None when the
/// call never closes.
fn read_call_args(chars_after_open: &str) -> Option<&str> {
    let mut depth = 1i32;
    let mut in_single = false;
    let mut in_double = false;
    for (at, c) in chars_after_open.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double => {
                depth -= 1;
                if depth == 0 {
                    return Some(&chars_after_open[..at]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The mode an open call names.
///
/// The mode is the second positional argument or the mode keyword. A keyword
/// call leaves the file keyword and the mode keyword, so the positional count
/// is what distinguishes them.
fn open_mode(args: &str) -> OpenMode {
    let mut positional = 0usize;
    for arg in split_call_args(args) {
        let arg = arg.trim();
        if let Some(rest) = arg.strip_prefix("mode")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return match literal_value(value.trim()) {
                Some(mode) => OpenMode::Literal(mode),
                None => OpenMode::Opaque,
            };
        }
        if arg.contains('=')
            && !arg.starts_with('\'')
            && !arg.starts_with('"')
            && !arg.starts_with(['(', '[', '{'])
        {
            // A keyword argument this scan does not know the meaning of could
            // be the mode, so it is not proof of reading.
            if arg.starts_with("encoding") || arg.starts_with("errors") || arg.starts_with("file") {
                continue;
            }
            return OpenMode::Opaque;
        }
        positional += 1;
        if positional == 2 {
            return match literal_value(arg) {
                Some(mode) => OpenMode::Literal(mode),
                None => OpenMode::Opaque,
            };
        }
    }
    OpenMode::Absent
}

/// Split an argument list on its top-level commas.
fn split_call_args(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut in_single = false;
    let mut in_double = false;
    for (at, c) in args.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '(' | '[' | '{' if !in_single && !in_double => depth += 1,
            ')' | ']' | '}' if !in_single && !in_double => depth -= 1,
            ',' if !in_single && !in_double && depth == 0 => {
                out.push(&args[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    out.push(&args[start..]);
    out
}

/// The text of a string literal, or None when the value is not one.
fn literal_value(value: &str) -> Option<String> {
    let quote = value.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let rest = value.get(quote.len_utf8()..)?;
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests;
