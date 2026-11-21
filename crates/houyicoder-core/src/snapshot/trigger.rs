//! Heuristic classifier deciding whether a shell command warrants a workspace
//! undo snapshot before running.
//!
//! The permission gate performs authoritative safety decisions in its own
//! crate; this module stays in the core layer and avoids taking snapshots on
//! read-only commands, descriptor duplications, or scratch-only cleanups.

use std::mem::take;

use crate::agent::git_safety::command_triggers_git_snapshot;

/// Return whether a shell command can delete or overwrite workspace files and
/// therefore needs a copy-on-write snapshot before execution.
pub fn is_destructive_command(command: &str) -> bool {
    if command_triggers_git_snapshot(command) {
        return true;
    }
    let stripped = strip_heredoc_bodies(command);
    let scan = scan_shell(&stripped);
    if scan
        .redirects
        .iter()
        .any(|target| !is_discard_or_temp(target))
    {
        return true;
    }
    scan.clauses
        .iter()
        .any(|words| clause_is_destructive(words))
}

#[derive(Default)]
struct ShellScan {
    clauses: Vec<Vec<String>>,
    current_clause: Vec<String>,
    redirects: Vec<String>,
    token: String,
    take_redirect: bool,
    skip_word: bool,
    /// Whether the token has begun, which an empty quoted word does even
    /// though it contributes no characters. Without this, an operand such as
    /// an empty quoted string leaves a pending skip_word in place, and the
    /// next word the scan swallows is a command rather than an operand.
    word_started: bool,
}

impl ShellScan {
    fn push_char(&mut self, ch: char) {
        self.word_started = true;
        self.token.push(ch);
    }

    fn flush_word(&mut self) {
        if !self.word_started {
            return;
        }
        self.word_started = false;
        let word = take(&mut self.token);
        if self.skip_word {
            self.skip_word = false;
        } else if self.take_redirect {
            self.take_redirect = false;
            self.redirects.push(word);
        } else {
            self.current_clause.push(word);
        }
    }

    fn flush_clause(&mut self) {
        self.flush_word();
        if !self.current_clause.is_empty() {
            self.clauses.push(take(&mut self.current_clause));
        }
    }
}

fn scan_shell(input: &str) -> ShellScan {
    let mut state = ShellScan::default();
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                state.push_char(ch);
            }
            i += 1;
        } else if in_double {
            if let Some(step) = line_continuation_len(&chars, i) {
                i += step;
            } else if ch == '\\' && i + 1 < chars.len() {
                state.push_char(chars[i + 1]);
                i += 2;
            } else {
                if ch == '"' {
                    in_double = false;
                } else {
                    state.push_char(ch);
                }
                i += 1;
            }
        } else {
            i = step_unquoted(&chars, i, &mut state, &mut in_single, &mut in_double);
        }
    }
    state.flush_clause();
    state
}

fn line_continuation_len(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i) != Some(&'\\') {
        return None;
    }
    match chars.get(i + 1) {
        Some('\n') => Some(2),
        Some('\r') if chars.get(i + 2) == Some(&'\n') => Some(3),
        _ => None,
    }
}

fn step_unquoted(
    chars: &[char],
    i: usize,
    state: &mut ShellScan,
    in_single: &mut bool,
    in_double: &mut bool,
) -> usize {
    if let Some(step) = line_continuation_len(chars, i) {
        return i + step;
    }
    let ch = chars[i];
    match ch {
        '\\' if i + 1 < chars.len() => {
            state.push_char(chars[i + 1]);
            i + 2
        }
        '#' if !state.word_started => {
            // An unquoted comment runs to the end of the line without opening
            // quotes or redirects inside its prose.
            let mut next = i + 1;
            while next < chars.len() && chars[next] != '\n' {
                next += 1;
            }
            next
        }
        '\'' => {
            state.word_started = true;
            *in_single = true;
            i + 1
        }
        '"' => {
            state.word_started = true;
            *in_double = true;
            i + 1
        }
        '\n' => {
            // A newline ends a command exactly as a semicolon does. Reading it
            // as ordinary whitespace joins the lines into one clause, and a
            // clause is judged by its first word, so a delete on a later line
            // goes unseen.
            state.flush_clause();
            i + 1
        }
        c if c.is_ascii_whitespace() => {
            state.flush_word();
            i + 1
        }
        '{' if state.token.ends_with('$') => {
            state.push_char('{');
            let mut next = i + 1;
            while next < chars.len() && chars[next] != '}' {
                state.push_char(chars[next]);
                next += 1;
            }
            if next < chars.len() {
                state.push_char('}');
                next += 1;
            }
            next
        }
        ';' | '|' | '(' | ')' | '{' | '}' | '`' => {
            state.flush_clause();
            i + 1
        }
        '&' => step_ampersand(chars, i, state),
        '>' => step_greater(chars, i, state),
        '<' => step_less(chars, i, state),
        _ => {
            state.push_char(ch);
            i + 1
        }
    }
}

fn step_ampersand(chars: &[char], i: usize, state: &mut ShellScan) -> usize {
    if i + 1 < chars.len() && chars[i + 1] == '>' {
        state.flush_word();
        let mut next = i + 2;
        if next < chars.len() && chars[next] == '>' {
            next += 1;
        }
        state.take_redirect = true;
        next
    } else {
        state.flush_clause();
        i + usize::from(i + 1 < chars.len() && chars[i + 1] == '&') + 1
    }
}

fn step_greater(chars: &[char], i: usize, state: &mut ShellScan) -> usize {
    let fd_prefix = !state.token.is_empty() && state.token.chars().all(|c| c.is_ascii_digit());
    if fd_prefix {
        // The digits belong to the operator, not to a word, so they leave no
        // empty word behind that would shadow the command word that follows.
        state.token.clear();
        state.word_started = false;
    } else if state.token.ends_with('-') || state.token.ends_with('=') {
        state.push_char('>');
        return i + 1;
    } else {
        state.flush_word();
    }
    if i + 1 < chars.len() && chars[i + 1] == '&' {
        // Descriptor duplication such as 2>&1 or >&2 writes no file.
        let mut next = i + 2;
        while next < chars.len() && (chars[next].is_ascii_digit() || chars[next] == '-') {
            next += 1;
        }
        next
    } else {
        let mut next = i + 1;
        if next < chars.len() && (chars[next] == '>' || chars[next] == '|') {
            next += 1;
        }
        state.take_redirect = true;
        next
    }
}

fn step_less(chars: &[char], i: usize, state: &mut ShellScan) -> usize {
    state.flush_word();
    let mut next = i + 1;
    while next < chars.len() && (chars[next] == '<' || chars[next] == '-') {
        next += 1;
    }
    state.skip_word = true;
    next
}

fn clause_is_destructive(words: &[String]) -> bool {
    let mut idx = 0;
    while idx < words.len() {
        let word = words[idx].as_str();
        if is_env_assignment(word) {
            idx += 1;
            continue;
        }
        let base = word.rsplit('/').next().unwrap_or(word);
        if base.eq_ignore_ascii_case("sudo") {
            return true;
        }
        if matches!(
            base,
            "env" | "command" | "nohup" | "nice" | "time" | "xargs"
        ) {
            idx = skip_flags(words, idx + 1);
            continue;
        }
        if base == "timeout" {
            idx = skip_timeout_args(words, idx + 1);
            continue;
        }
        if matches!(base, "rm" | "rmdir" | "unlink") {
            let operands: Vec<&str> = words[idx + 1..]
                .iter()
                .map(String::as_str)
                .filter(|w| !w.starts_with('-'))
                .collect();
            return operands.is_empty() || operands.iter().any(|op| !is_temp_target(op));
        }
        return false;
    }
    false
}

fn skip_flags(words: &[String], mut idx: usize) -> usize {
    while idx < words.len() && words[idx].starts_with('-') {
        idx += 1;
    }
    idx
}

fn skip_timeout_args(words: &[String], mut idx: usize) -> usize {
    while idx < words.len() && words[idx].starts_with('-') {
        let flag = words[idx].as_str();
        if flag == "--" {
            idx += 1;
            break;
        }
        if matches!(flag, "-k" | "-s" | "--kill-after" | "--signal") {
            idx = (idx + 2).min(words.len());
        } else {
            idx += 1;
        }
    }
    (idx + 1).min(words.len())
}

fn is_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_discard_or_temp(target: &str) -> bool {
    target == "/dev/null" || is_temp_target(target)
}

fn is_temp_target(target: &str) -> bool {
    if target.contains("..") {
        return false;
    }
    target == "$TMPDIR"
        || target == "${TMPDIR}"
        || target.starts_with("$TMPDIR/")
        || target.starts_with("${TMPDIR}/")
        || target.starts_with("/tmp/")
        || target.starts_with("/private/tmp/")
        || target.starts_with("/var/tmp/")
}

fn strip_heredoc_bodies(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut stop: Option<String> = None;
    for line in command.split_inclusive('\n') {
        if let Some(delim) = stop.as_deref() {
            if line.trim() == delim {
                stop = None;
            }
            continue;
        }
        out.push_str(line);
        stop = find_heredoc_delimiter(line);
    }
    out
}

fn find_heredoc_delimiter(line: &str) -> Option<String> {
    let pos = line.find("<<")?;
    let rest = &line[pos + 2..];
    if rest.starts_with('<') {
        return None;
    }
    let rest = rest.strip_prefix('-').unwrap_or(rest).trim_start();
    let trimmed = rest.trim_start_matches(['\'', '"', '\\']);
    let end = trimmed
        .find(|c: char| c.is_ascii_whitespace() || matches!(c, '\'' | '"' | ';' | '&' | '|'))
        .unwrap_or(trimmed.len());
    let delim = &trimmed[..end];
    (!delim.is_empty()).then(|| delim.to_string())
}
