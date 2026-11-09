//! The shell construct unwrapper: peels the control-flow and command-
//! substitution shells around a clause so the classifiers below see the
//! atomic commands. A loop header, a case label, and an assignment's
//! substitution body are not programs; the programs inside them are what
//! the effect and egress classifiers must judge. Peeling is deterministic
//! and quote-aware, and anything it cannot prove benign stays unpeeled so
//! the fail-closed classifiers above it still hold the line.

use std::mem::take;

/// A loop variable name is an identifier: letters, digits, underscores,
/// starting with a letter or underscore.
fn is_identifier(word: &str) -> bool {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The environment names whose assignment changes how a request tool
/// behaves: a hijacked config or proxy redirects where the traffic lands,
/// so a peeling that drops them would hand the request to a different
/// destination than the one the classifiers judged.
const REFUSED_ENV_NAMES: &[&str] = &[
    "CURL_HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "GIT_SSH_COMMAND",
    "LD_PRELOAD",
    "PATH",
    "TMPDIR",
    "ENV",
    "BASH_ENV",
];

/// Replace each substitution with a marker word, quote-aware: a
/// substitution inside double quotes is still a substitution to the shell,
/// so it is replaced there too. A backquote substitution refuses the
/// clause (the older form this parser does not track).
fn replace_substitutions_with_marker(clause: &str) -> Option<String> {
    let mut replaced = String::new();
    let chars: Vec<char> = clause.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\'' => {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i = (i + 1).min(chars.len());
                replaced.extend(&chars[start..i]);
            }
            '"' => {
                let start = i;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        break;
                    }
                    if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
                        i = advance_past_body(&chars, i + 1);
                        replaced.push_str(" CMD ");
                        continue;
                    }
                    i += 1;
                }
                i = (i + 1).min(chars.len());
                replaced.extend(&chars[start..i]);
            }
            '`' => return None,
            '$' if i + 1 < chars.len() && chars[i + 1] == '(' => {
                i = advance_past_body(&chars, i + 1);
                replaced.push_str(" CMD ");
            }
            c => {
                replaced.push(c);
                i += 1;
            }
        }
    }
    Some(replaced)
}

/// Peel one clause's shell constructs, returning the atomic program text
/// the classifiers should judge. Returns None when the clause contains a
/// construct this unwrapper cannot prove benign — process substitution,
/// dynamic eval — so the caller holds the clause rather than guessing.
///
/// Substitution bodies are extracted first and replaced by a bare marker
/// word, so the paren spans never split across words; the bodies themselves
/// are returned separately by substitution_bodies for the caller's
/// recursive walk.
pub(crate) fn unwrap_clause(clause: &str) -> Option<String> {
    // Replace each substitution with a marker word so a pipeline inside
    // the parens survives as one clean gap rather than a split word. The
    // bodies are judged by the caller's recursion, not by this peel.
    let replaced = replace_substitutions_with_marker(clause)?;
    let words = split_words_quote_aware(&replaced);
    let mut kept: Vec<String> = Vec::new();
    // The case head's tested word is data, and the label after in is too:
    // both track through these two flags.
    let mut in_case_head = false;
    let mut after_case_in = false;
    let mut in_loop_head = false;
    for word in &words {
        // Assignment and keyword checks read the word's bare text, but the
        // kept word keeps its quotes: the classifiers below parse argument
        // lists, and a quoted header value or URL must stay one word.
        let bare = strip_word_quotes(word);
        // A leading assignment: NAME=value with a plain value. The value's
        // substitution was already replaced by the marker word, so what
        // remains is either a plain assignment or the marker itself. A name
        // that changes how the request tools behave refuses the clause:
        // peeling it away would judge a destination the assignment has
        // already redirected.
        if let Some(eq) = bare.find('=')
            && is_identifier(&bare[..eq])
            && !bare[eq + 1..].contains(' ')
        {
            if REFUSED_ENV_NAMES.contains(&&bare[..eq]) {
                return None;
            }
            continue;
        }
        // The substitution marker: its body is judged by the caller. At the
        // head of a clause it stands for a command position (a loop's
        // generator, an assignment's capture), so it peels and the body
        // recurses. Mid-clause it was part of an argument — a URL's query,
        // a header value — and that argument is no longer literal, so the
        // whole clause refuses rather than judging a rewritten destination.
        if bare == "CMD" {
            if kept.is_empty() {
                continue;
            }
            return None;
        }
        // A case pattern label or its separator, at the head of a branch.
        // The case head's tested word is data too, so between case and in
        // every word peels; the first label after in peels the same way.
        if kept.is_empty() && case_label_tail(word).is_some() {
            continue;
        }
        let bare = bare.trim_end_matches(';').trim_matches(|c: char| c == ';');
        let bare = bare.trim_matches(|c: char| c == ';');
        match bare {
            "case" => {
                in_case_head = true;
                continue;
            }
            "in" if in_case_head => {
                in_case_head = false;
                after_case_in = true;
                continue;
            }
            _ if in_case_head => continue,
            _ if after_case_in => continue,
            "for" => {
                // The loop's variable name and the in that follows are
                // header structure: the words between for and in peel so
                // the marker after in lands at the head where it belongs.
                in_loop_head = true;
                continue;
            }
            "in" if in_loop_head => {
                in_loop_head = false;
                continue;
            }
            _ if in_loop_head => continue,
            "while" | "until" | "if" | "then" | "elif" | "else" | "fi" | "do" | "done" | "esac"
            | "break" | "continue" | "in" | ";;" => continue,
            "" => continue,
            _ => kept.push(word.clone()),
        }
    }
    if kept.is_empty() {
        // A clause of pure structure (a stray done) judges as empty: no
        // program, no effect.
        return Some(String::new());
    }
    Some(kept.join(" "))
}

/// The text after a case pattern label's closing paren, when the word is a
/// label at all. A label ends in a paren and opens none.
fn case_label_tail(word: &str) -> Option<&str> {
    if word.ends_with(')') && !word.starts_with('(') {
        Some("")
    } else {
        None
    }
}

/// Extract every command-substitution body in the clause, quote-aware.
/// Returns None when a backquote substitution is present (the older form
/// this parser does not track) so the caller refuses rather than guessing.
pub(crate) fn substitution_bodies(clause: &str) -> Option<Vec<String>> {
    let mut bodies = Vec::new();
    let chars: Vec<char> = clause.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                i += 2;
                continue;
            }
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
                continue;
            }
            '"' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        break;
                    }
                    // A substitution runs inside double quotes too.
                    if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
                        let body = read_paren_body(&chars, i + 1)?;
                        bodies.push(body);
                        i = advance_past_body(&chars, i + 1);
                        continue;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            '`' => return None,
            '$' if i + 1 < chars.len() && chars[i + 1] == '(' => {
                let body = read_paren_body(&chars, i + 1)?;
                bodies.push(body);
                i = advance_past_body(&chars, i + 1);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    Some(bodies)
}

/// Read the text between a balanced paren pair, starting at the open paren.
/// Quote-aware: a paren inside quotes is text, not nesting, so a body that
/// carries Python calls with their own parens keeps them. Returns None on
/// an unbalanced pair: a malformed clause is refused, not guessed.
fn read_paren_body(chars: &[char], open: usize) -> Option<String> {
    let mut depth = 0i32;
    let mut body = String::new();
    let mut i = open;
    let mut in_single = false;
    let mut in_double = false;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                body.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                body.push(c);
            }
            '\\' if i + 1 < chars.len() && in_double => {
                body.push(c);
                if let Some(next) = chars.get(i + 1) {
                    body.push(*next);
                }
                i += 2;
                continue;
            }
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double => {
                depth -= 1;
                if depth == 0 {
                    return Some(body);
                }
            }
            _ => body.push(c),
        }
        i += 1;
    }
    None
}

/// The index just past a balanced paren pair, starting at the open paren.
/// Quote-aware to match read_paren_body, so the pair end both compute is
/// the same index.
fn advance_past_body(chars: &[char], open: usize) -> usize {
    let mut depth = 0i32;
    let mut i = open;
    let mut in_single = false;
    let mut in_double = false;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\\' if i + 1 < chars.len() && in_double => {
                i += 2;
                continue;
            }
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

/// Split on whitespace outside quotes. A word keeps its quotes so the
/// caller can strip them deliberately.
fn split_words_quote_aware(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for c in text.chars() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                cur.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    words.push(take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Strip one layer of surrounding quotes from a word.
fn strip_word_quotes(word: &str) -> String {
    let mut chars: Vec<char> = word.chars().collect();
    while chars.len() >= 2
        && ((chars.first() == Some(&'"') && chars.last() == Some(&'"'))
            || (chars.first() == Some(&'\'') && chars.last() == Some(&'\'')))
    {
        chars = chars[1..chars.len() - 1].to_vec();
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests;
