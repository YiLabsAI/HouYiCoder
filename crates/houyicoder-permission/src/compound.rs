//! Compound-command safety. A compound shell command (A && B || C) is only
//! auto-allowable when every segment is independently attestable: no file
//! redirect, no command substitution, no heredoc. A single un-attestable
//! segment escalates the whole command to Ask so the user inspects it. The
//! gate never auto-allows a compound command it cannot statically attest.
//!
//! The checks here are structural heuristics, not a full shell parser — they
//! catch the high-risk constructs (redirects, substitution, heredoc, process
//! substitution) that change which resources a command touches. A full grammar
//! is out of scope; the gate escalates anything ambiguous to Ask.

use crate::heredoc::strip_quoted_heredoc_bodies;

mod effect;
mod fence_write;
mod readonly;

pub use effect::{is_effect_safe, reads_only_protected, removes_only_temp};
pub(crate) use effect::{is_temp_output_target, sets_tmpdir, starts_with_supported_command};
pub(crate) use fence_write::writes_fence_denied_path;
pub use readonly::is_readonly_compound;

/// Split a compound command into its top-level segments on and/or, semicolon,
/// pipe, newline, and background ampersand. A bare pipe counts as a segment
/// boundary (each stage of a pipeline is a separate attestable unit). Empty
/// segments are dropped. Quote- and paren-aware: a separator inside quotes or
/// inside a command substitution $(...) or group (...) is not a boundary, so a
/// pipe inside a substitution stays in one segment. Newline and a single
/// ampersand are boundaries because the shell runs them as such: a segment that
/// carries one would describe a one-command program while the shell runs two.
/// A line-continuation backslash joins the next line instead, and an ampersand
/// that joins a redirect (&> or >&) stays part of the segment. A heredoc body
/// is input rather than commands, so its lines are not boundaries either. A
/// full grammar is out of scope; the gate escalates anything ambiguous to Ask.
pub fn split_compound(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    let mut heredoc: Option<(String, bool)> = None;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(c);
                i += 1;
                continue;
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(c);
                i += 1;
                continue;
            }
            _ => {}
        }
        if in_single || in_double {
            cur.push(c);
            i += 1;
            continue;
        }
        // Track parens so a separator inside $(...) or (...) does not split.
        if c == '(' {
            depth += 1;
            cur.push(c);
            i += 1;
            continue;
        }
        if c == ')' {
            if depth > 0 {
                depth -= 1;
            }
            cur.push(c);
            i += 1;
            continue;
        }
        if depth > 0 {
            cur.push(c);
            i += 1;
            continue;
        }
        // A heredoc reads its body as input rather than running it, so the
        // body is not a command list and its lines are not boundaries. The
        // delimiter is remembered here and the body is skipped when the
        // current line ends.
        if c == '<' && chars.get(i + 1) == Some(&'<') {
            let (next, found) = read_heredoc(&chars, i, &mut cur);
            i = next;
            heredoc = found.or(heredoc);
            continue;
        }
        // and/or operators: double ampersand or double pipe.
        if (c == '&' || c == '|') && i + 1 < chars.len() && chars[i + 1] == c {
            push_seg(&mut out, &cur);
            cur.clear();
            i += 2;
            continue;
        }
        // A newline ends a command the way a semicolon does; a backslash
        // before it continues the same command on the next line. A heredoc
        // body that starts here is skipped up to its delimiter line.
        if c == '\n' {
            if cur.ends_with('\\') {
                cur.pop();
            } else {
                push_seg(&mut out, &cur);
                cur.clear();
            }
            i += 1;
            if let Some((delim, strip_tabs)) = heredoc.take() {
                i = skip_heredoc_body(&chars, i, &delim, strip_tabs);
            }
            continue;
        }
        // A single ampersand starts the next command once the left one is
        // running, so it is a boundary too. It stays in the segment when it
        // joins a redirect: &> sends both streams, >& duplicates one.
        if c == '&' && chars.get(i + 1) != Some(&'>') && !cur.ends_with('>') {
            push_seg(&mut out, &cur);
            cur.clear();
            i += 1;
            continue;
        }
        // single semicolon or pipe as a boundary.
        if c == ';' || c == '|' {
            push_seg(&mut out, &cur);
            cur.clear();
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    push_seg(&mut out, &cur);
    out
}

/// Read a heredoc operator at start, appending it to the segment as written.
/// Returns where scanning resumes, one past the delimiter word, and the
/// delimiter the body runs to. A bare here-string (three angle brackets) or a
/// word with no delimiter leaves the body unknown, so no delimiter is
/// reported and the rest of the line scans as ordinary text.
fn read_heredoc(chars: &[char], start: usize, cur: &mut String) -> (usize, Option<(String, bool)>) {
    cur.push('<');
    cur.push('<');
    let mut i = start + 2;
    let strip_tabs = chars.get(i) == Some(&'-');
    if strip_tabs {
        cur.push('-');
        i += 1;
    }
    let word_start = i;
    while i < chars.len()
        && !chars[i].is_whitespace()
        && !matches!(chars[i], ';' | '|' | '&' | '<' | '>' | '(' | ')')
    {
        cur.push(chars[i]);
        i += 1;
    }
    let word: String = chars[word_start..i].iter().collect();
    let name = word.trim_matches(|c| c == '\'' || c == '"');
    let delim = (!name.is_empty()).then(|| (name.to_string(), strip_tabs));
    (i, delim)
}

/// The position after a heredoc body. The first line whose text equals the
/// delimiter ends the body; a tab-stripping heredoc (<<-) ignores leading tabs
/// on that line, matching how the shell compares it. A body that never
/// reaches its delimiter runs to the end of the input.
fn skip_heredoc_body(chars: &[char], mut i: usize, delim: &str, strip_tabs: bool) -> usize {
    while i < chars.len() {
        let line_start = i;
        while i < chars.len() && chars[i] != '\n' {
            i += 1;
        }
        let line: String = chars[line_start..i].iter().collect();
        let line = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        i += 1;
        if line == delim {
            break;
        }
    }
    i
}

fn push_seg(out: &mut Vec<String>, s: &str) {
    let t = s.trim();
    if !t.is_empty() {
        out.push(t.into());
    }
}

/// Split a command line into words the way the shell does, keeping each
/// word's quotes and escapes. A quoted run of text is one word even when it
/// contains spaces, and an unterminated quote leaves the rest of the line as
/// one word the caller's decoder then refuses. Splitting on whitespace alone
/// tears a quoted argument apart and reads the halves as syntax errors that
/// are not there.
pub(crate) fn shell_word_spans(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if let Some(open) = quote {
            if byte == b'\\' && open == b'"' {
                i += 2;
                continue;
            }
            if byte == open {
                quote = None;
            }
            i += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' => {
                quote = Some(byte);
                start.get_or_insert(i);
                i += 1;
            }
            b'\\' => {
                start.get_or_insert(i);
                i += 2;
            }
            byte if byte.is_ascii_whitespace() => {
                if let Some(from) = start.take() {
                    spans.push(&text[from..i]);
                }
                i += 1;
            }
            _ => {
                start.get_or_insert(i);
                i += 1;
            }
        }
    }
    if let Some(from) = start {
        spans.push(&text[from..]);
    }
    spans
}

/// Resolve the quotes and escapes in one word kept by shell_word_spans, so a
/// caller compares the text the shell passes to the process rather than the
/// spelling on the line. None when the word holds an expansion or an
/// unterminated quote: neither can be shown to name a fixed path.
pub(super) fn decode_shell_word(raw: &str) -> Option<String> {
    if raw.contains(['$', '`']) {
        return None;
    }
    let mut out = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if c == '\\' && !in_single {
            let next = *chars.get(i + 1)?;
            out.push(next);
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    if in_single || in_double {
        None
    } else {
        Some(out)
    }
}

/// Whether a word is a literal environment assignment (NAME=value) rather
/// than a command, an argument, or a dynamic expansion. The name must be a
/// plain identifier and the value must carry no expansion or command
/// substitution, so an assignment that executes a subshell is never stripped
/// ahead of the detectors.
pub(crate) fn is_env_assignment(word: &str) -> bool {
    let Some((name, value)) = word.split_once('=') else {
        return false;
    };
    if matches!(
        name,
        "TMPDIR"
            | "TMPPREFIX"
            | "HOME"
            | "CURL_HOME"
            | "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "ALL_PROXY"
            | "http_proxy"
            | "https_proxy"
            | "all_proxy"
            | "GIT_SSH_COMMAND"
            | "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "DYLD_INSERT_LIBRARIES"
            | "DYLD_LIBRARY_PATH"
    ) || name.contains('[')
    {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !value.contains(['$', '`', '(', ')', '\\', ';', '|', '&', '<', '>'])
}

/// Whether a whole segment only sets environment variables, so it reaches no
/// program and no resource.
fn is_assignment_only(segment: &str) -> bool {
    let mut words = segment.split_whitespace();
    let first = words.next();
    first.is_some_and(is_env_assignment) && words.all(is_env_assignment)
}

/// Drop the environment prefix a command carries, so a rule and the call it
/// governs describe the same program. Without this a rule derived from a call
/// that begins with an assignment names the assignment, which no other call
/// shares, and a rule for the real program never matches a call that sets a
/// variable first.
///
/// Leading assignment-only segments are dropped whole; the leading assignment
/// words of the first live segment are dropped from that segment. The rest of
/// the command is returned unchanged, separators included, so a rule derived
/// from the result still describes the same shell text.
pub fn normalize_shell_command(command: &str) -> String {
    let mut cursor = 0;
    let mut keep_from = None;
    for segment in split_compound(command) {
        let Some(offset) = command[cursor..].find(segment.as_str()) else {
            // The parsed segment is not a contiguous slice of the input, which
            // means the split lost track of the original text. Leave the
            // command alone rather than rewrite it into something else.
            return command.trim().to_string();
        };
        let at = cursor + offset;
        cursor = at + segment.len();
        if keep_from.is_some() || is_assignment_only(&segment) {
            continue;
        }
        keep_from = Some(at + assignment_prefix_len(&segment));
    }
    match keep_from {
        Some(at) => command[at..].trim().to_string(),
        None => String::new(),
    }
}

/// The byte length of the leading run of environment assignments in one
/// segment, including the whitespace after each.
fn assignment_prefix_len(segment: &str) -> usize {
    let mut consumed = 0;
    let mut rest = segment;
    loop {
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if !is_env_assignment(&rest[..word_end]) {
            return consumed;
        }
        if word_end == rest.len() {
            return segment.len();
        }
        let gap = rest[word_end..].len() - rest[word_end..].trim_start().len();
        consumed += word_end + gap;
        rest = &rest[word_end + gap..];
    }
}

/// Strip the redirect forms that do NOT touch a real file, so the
/// attestability scan only escalates on a redirect to a path. Four forms are
/// safe: 2>&1 (stderr to stdout), an optional source fd then > then /dev/null
/// (discard stdout), < then /dev/null (discard stdin), and [01]? >& 2 (stdout
/// to stderr). The trailing boundary is mandatory — a redirect to /dev/nullo
/// must NOT match /dev/null as a prefix, else the strip would hide a real file
/// write and the redirect check would pass. The expression here is a
/// hand-rolled char scan.
pub(crate) fn strip_safe_redirects(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if let Some(end) = match_safe_redirect(&chars, i) {
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Try to match a safe-redirect form at chars[start..], returning the position
/// one past the match (the caller skips the whole match). Returns None when no
/// form matches or the trailing boundary (whitespace, end-of-input, or a
/// separator) fails. NOT quote-aware: a redirect inside a quoted string is
/// rare data, an acceptable approximation.
fn match_safe_redirect(chars: &[char], start: usize) -> Option<usize> {
    let at = |i: usize| chars.get(i).copied();

    // Form A: 2 >& 1 — stderr to stdout. Source fd is literally 2, dest 1;
    // whitespace tolerated between every token. A leading whitespace run is
    // not required; the attestability result does not depend on it — only
    // the trailing boundary is load-bearing for correctness.
    if at(start) == Some('2') {
        let mut j = start + 1;
        while at(j) == Some(' ') || at(j) == Some('\t') {
            j += 1;
        }
        if at(j) == Some('>') {
            let mut k = j + 1;
            while at(k) == Some(' ') || at(k) == Some('\t') {
                k += 1;
            }
            if at(k) == Some('&') {
                let mut m = k + 1;
                while at(m) == Some(' ') || at(m) == Some('\t') {
                    m += 1;
                }
                if at(m) == Some('1') && is_boundary(at(m + 1)) {
                    return Some(m + 1);
                }
            }
        }
    }

    // Form B: [012]? > /dev/null — discard stdout, optional source fd.
    let mut j = start;
    if matches!(at(j), Some('0' | '1' | '2')) {
        j += 1;
    }
    while at(j) == Some(' ') || at(j) == Some('\t') {
        j += 1;
    }
    if at(j) == Some('>') {
        let mut k = j + 1;
        while at(k) == Some(' ') || at(k) == Some('\t') {
            k += 1;
        }
        if matches_dev_null(chars, k) && is_boundary(at(k + DEV_NULL_LEN)) {
            return Some(k + DEV_NULL_LEN);
        }
    }

    // Form C: < /dev/null — discard stdin. Tolerate leading whitespace so a
    // space before < is consumed with the redirect.
    let mut j = start;
    while at(j) == Some(' ') || at(j) == Some('\t') {
        j += 1;
    }
    if at(j) == Some('<') {
        let mut k = j + 1;
        while at(k) == Some(' ') || at(k) == Some('\t') {
            k += 1;
        }
        if matches_dev_null(chars, k) && is_boundary(at(k + DEV_NULL_LEN)) {
            // The leading whitespace (j - start) is part of the stripped span.
            return Some(k + DEV_NULL_LEN);
        }
    }

    // Form D: [01]? >& 2 — stdout to stderr, the mirror of form A. It names
    // no file either, so a script that splits its streams reads as
    // attestable. A command that sends both streams to a file spells that as
    // &> path, whose ampersand comes before the angle and never reaches here.
    let mut j = start;
    if matches!(at(j), Some('0' | '1')) {
        j += 1;
    }
    while at(j) == Some(' ') || at(j) == Some('\t') {
        j += 1;
    }
    if at(j) == Some('>') {
        let mut k = j + 1;
        while at(k) == Some(' ') || at(k) == Some('\t') {
            k += 1;
        }
        if at(k) == Some('&') {
            let mut m = k + 1;
            while at(m) == Some(' ') || at(m) == Some('\t') {
                m += 1;
            }
            if at(m) == Some('2') && is_boundary(at(m + 1)) {
                return Some(m + 1);
            }
        }
    }

    None
}

const DEV_NULL_LEN: usize = 9;

/// True when the slice at start matches the literal /dev/null char sequence.
fn matches_dev_null(chars: &[char], start: usize) -> bool {
    const DEV_NULL: [char; DEV_NULL_LEN] = ['/', 'd', 'e', 'v', '/', 'n', 'u', 'l', 'l'];
    if start + DEV_NULL_LEN > chars.len() {
        return false;
    }
    chars[start..start + DEV_NULL_LEN] == DEV_NULL
}

/// The trailing boundary a safe redirect must satisfy: followed by whitespace,
/// the end of the input, or a separator that ends the segment on the spot.
///
/// The separators matter because a redirect is often the last thing before
/// one: a group closes with a paren, and a chain continues with a pipe or a
/// semicolon. A quote is deliberately not a boundary. Quoting concatenates
/// mid-word, so a redirect followed by a quoted fragment names the file the
/// concatenation spells, and treating the quote as a boundary would strip a
/// redirect that writes a real path.
fn is_boundary(next: Option<char>) -> bool {
    matches!(
        next,
        None | Some(' ' | '\t' | '\n' | '\r' | ')' | ';' | '|')
    )
}

/// Whether a single command segment is structurally attestable: free of file
/// redirects, command substitution, unquoted heredoc, and process
/// substitution outside quotes. A redirect or substitution inside quotes
/// is data, not an operator.
///
/// Safe redirect forms (2>&1, > /dev/null, < /dev/null, [01]? >& 2) are
/// stripped first so cargo test 2>&1 reads as attestable while
/// cargo test > log.txt escalates.
/// Quoted heredoc bodies (<<'DELIM') are stripped to a placeholder so body
/// content does not affect attestability; unquoted heredoc (<<DELIM) keeps
/// bash expansion and stays un-attestable. The << operator itself is not a
/// file redirect, so the quoted form does not escalate; the unquoted form
/// does because the body may hide expansion.
pub fn is_attestable(segment: &str) -> bool {
    let stripped = strip_quoted_heredoc_bodies(&strip_safe_redirects(segment));
    let mut scan = QuoteScan::new(&stripped);
    while let Some(c) = scan.next() {
        if scan.in_single {
            continue;
        }
        if c == '`' {
            return false;
        }
        // Command substitution runs inside double quotes as well as unquoted.
        if c == '$' && scan.peek_next() == Some('(') {
            return false;
        }
        if scan.in_double {
            continue;
        }
        // Quoted heredoc (<< with quote, dash, or backslash after) feeds
        // literal text; body already stripped. Skip so only single <
        // (file input redirect) escalates.
        if c == '<' && scan.peek_next() == Some('<') {
            let after = scan.chars.get(scan.pos + 1).copied();
            if matches!(after, Some('\'') | Some('"') | Some('-') | Some('\\')) {
                scan.next();
                continue;
            }
            return false;
        }
        if c == '>' || c == '<' {
            return false;
        }
    }
    // An unterminated quote leaves two scanners disagreeing about where a word
    // ends, and the shell's reading is the one that runs.
    !scan.in_single && !scan.in_double
}

/// Whether every quote in the text is closed. A command whose quoting this
/// scan cannot follow is never attested as reading, so a compound split at a
/// separator the scan does not see cannot hide a second command.
pub(crate) fn quotes_balanced(text: &str) -> bool {
    let mut scan = QuoteScan::new(text);
    while scan.next().is_some() {}
    !scan.in_single && !scan.in_double
}

/// Whether all segments are independently attestable. This is the gate's
/// per-segment safety predicate: returning false escalates the whole compound
/// command to Ask.
pub fn compound_safe(segments: &[&str]) -> bool {
    if segments.is_empty() {
        return false;
    }
    segments.iter().all(|s| is_attestable(s))
}

/// A quote-aware char scanner. Tracks single and double quote state so
/// operators inside quotes are not mistaken for shell operators. Ambiguous
/// quoting (a single quote inside double quotes and vice versa) is handled by
/// the simple in-single / in-double toggle — good enough for the high-risk
/// constructs; ambiguous input still escalates to Ask.
struct QuoteScan {
    chars: Vec<char>,
    pos: usize,
    in_single: bool,
    in_double: bool,
}

impl QuoteScan {
    fn new(s: &str) -> Self {
        Self {
            chars: s.chars().collect(),
            pos: 0,
            in_single: false,
            in_double: false,
        }
    }

    fn next(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied()?;
        match c {
            '\'' if !self.in_double => self.in_single = !self.in_single,
            '"' if !self.in_single => self.in_double = !self.in_double,
            _ => {}
        }
        self.pos += 1;
        Some(c)
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
}

#[cfg(test)]
mod tests;
