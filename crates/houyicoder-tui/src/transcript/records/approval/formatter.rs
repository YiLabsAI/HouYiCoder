//! Reading the code an interpreter is handed beside a request.
//!
//! A formatter reads the response and prints part of it. It is the one
//! interpreter use this walk can vouch for, and only when the code names no
//! module and no primitive that could leave the machine or write a file. The
//! test is positive on purpose: a module this file has not named is refused,
//! so a spelling the author did not think of fails closed.

/// The command with every interpreter's formatter body replaced by a no-op.
///
/// None when one body is not a formatter. The body is removed rather than
/// skipped because it contains the separators the caller splits on: a JSON
/// one-liner carries semicolons and parentheses, and leaving them in would cut
/// the script into segments the walk would read as commands.
pub(super) fn without_formatter_code(command: &str) -> Option<String> {
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
        let Some((body, after)) = inline_body(&chars, index) else {
            out.push_str(&word);
            continue;
        };
        if !is_formatter_code(&body) {
            return None;
        }
        out.push_str(" true ");
        index = after;
    }
    Some(out)
}

fn is_interpreter(word: &str) -> bool {
    matches!(
        word.to_ascii_lowercase().as_str(),
        // Only Python: the check reads import statements, and an interpreter
        // without them has no form to read.
        "python" | "python2" | "python3"
    )
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
    // A double-quoted body escapes its quote with a backslash, so a
    // backslash skips the character after it: without this an escaped quote
    // inside a Python format string ends the body early, and the tail reads
    // as an unknown command in the outer walk.
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

/// The attributes of the standard-input module a formatter may read.
///
/// The module itself is permitted because reading the response through it is
/// the idiom, but its attribute surface is not: the frame accessor and the
/// module table are two ways back to every name the import list refused, and a
/// formatter never needs either.
const FORMATTER_SYS_ATTRS: &[&str] = &[
    "argv",
    "maxsize",
    "stderr",
    "stdin",
    "stdout",
    "version",
    "version_info",
];

/// Whether inline code only formats what it reads.
///
/// The rule mirrors the gate's: every import names a data or text module, and
/// no refused name appears. The lists are kept in step by the tests beside
/// each, which pin the same representative cases.
fn is_formatter_code(code: &str) -> bool {
    let lower = code.to_ascii_lowercase();
    const REFUSED: &[&str] = &[
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
        "open",
        "os",
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
    if REFUSED.iter().any(|name| names_word(&lower, name)) {
        return false;
    }
    imports_permitted(&lower) && sys_attributes_are_permitted(&lower)
}

fn imports_permitted(code: &str) -> bool {
    const MODULES: &[&str] = &[
        "base64",
        "collections",
        "csv",
        "datetime",
        "decimal",
        "difflib",
        "fractions",
        "functools",
        "hashlib",
        "itertools",
        "json",
        "math",
        "re",
        "statistics",
        "string",
        "sys",
        "textwrap",
        "time",
        "typing",
        "unicodedata",
    ];
    for statement in code.split([';', '\n']) {
        let statement = statement.trim();
        let names = if let Some(rest) = statement.strip_prefix("import ") {
            rest
        } else if let Some(rest) = statement.strip_prefix("from ") {
            rest
        } else {
            continue;
        };
        for name in names.split(',') {
            let path = name.split_whitespace().next().unwrap_or("");
            let top = path.split('.').next().unwrap_or("");
            if !MODULES.contains(&top) {
                return false;
            }
        }
    }
    true
}

fn names_word(haystack: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(at) = haystack[from..].find(name) {
        let start = from + at;
        let end = start + name.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        let opens = before.is_none_or(|c| !is_name_char(c));
        let closes = after.is_none_or(|c| !is_name_char(c));
        if opens && closes {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether every attribute read off the standard-input module is one a
/// formatter needs.
///
/// The module is permitted, its surface is not: one accessor reaches the
/// frames of every function on the stack, and the module table reaches every
/// name the import list refused. Reading a response never needs either.
fn sys_attributes_are_permitted(code: &str) -> bool {
    let mut from = 0;
    while let Some(at) = code[from..].find("sys.") {
        let start = from + at + "sys.".len();
        let rest = &code[start..];
        let end = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
        if !FORMATTER_SYS_ATTRS.contains(&&rest[..end]) {
            return false;
        }
        from = start + end.max(1);
    }
    true
}
