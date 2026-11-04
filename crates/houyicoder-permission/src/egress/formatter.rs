//! Reading the code an interpreter is handed downstream of a request.
//!
//! A formatter reads the response and prints part of it. It is the one
//! interpreter use this module can vouch for, and only when the code names no
//! module and no primitive that could leave the machine or write a file. The
//! test is positive on purpose: a module this file has not named is refused,
//! so a spelling the author did not think of fails closed.

/// The modules a formatter may import: data shapes, text handling, and number
/// handling. Everything else is absent, so an import of it fails the check.
const FORMATTER_MODULES: &[&str] = &[
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
    "operator",
    "re",
    "statistics",
    "string",
    "sys",
    "textwrap",
    "time",
    "typing",
    "unicodedata",
];

/// Names a formatter never needs. Each one is a way out: reaching the network,
/// starting a program, opening a file, or rebuilding a name the import list
/// cannot see. The scan is by whole word, so a longer identifier that merely
/// contains one is not a match.
const FORMATTER_REFUSED: &[&str] = &[
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

/// Whether interpreter code only formats what it reads.
///
/// Both halves are required: every import names a permitted module, and no
/// refused name appears. Code with no import at all passes the first half,
/// which is what a short one-liner looks like.
pub(super) fn is_pure_formatter(code: &str) -> bool {
    let lower = code.to_ascii_lowercase();
    if FORMATTER_REFUSED
        .iter()
        .any(|name| names_word(&lower, name))
    {
        return false;
    }
    imports_are_permitted(&lower) && sys_attributes_are_permitted(&lower)
}

/// Whether every import statement in the code names a permitted module.
///
/// Statements are read one at a time because an import is the only construct
/// that brings a capability in, and the first module of each name is what
/// decides: a submodule is a part of the module that was permitted.
fn imports_are_permitted(code: &str) -> bool {
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
            if !FORMATTER_MODULES.contains(&top) {
                return false;
            }
        }
    }
    true
}

/// Whether the text contains the name as a whole word.
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
