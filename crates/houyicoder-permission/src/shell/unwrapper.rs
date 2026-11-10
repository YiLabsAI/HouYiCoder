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

/// Replace each substitution with a marker, quote-aware: a substitution
/// inside double quotes is still a substitution to the shell, so it is
/// replaced there too. A backquote substitution refuses the clause (the older
/// form this parser does not track).
///
/// The quoted run is rebuilt in place rather than copied whole after a
/// marker: appending the original text pushed the marker outside the word it
/// came from, which cut a quoted argument into several words and refused every
/// clause that carried a substitution anywhere.
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
                replaced.push('"');
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        replaced.push(chars[i]);
                        replaced.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        replaced.push('"');
                        i += 1;
                        break;
                    }
                    if chars[i] == '`' {
                        return None;
                    }
                    if chars[i] == '$' && chars.get(i + 1) == Some(&'(') {
                        i = substitution_marker(&mut replaced, &chars, i, true)?;
                        continue;
                    }
                    replaced.push(chars[i]);
                    i += 1;
                }
            }
            '`' => return None,
            '$' if chars.get(i + 1) == Some(&'(') => {
                i = substitution_marker(&mut replaced, &chars, i, false)?;
            }
            // A process substitution runs its body in a subshell and hands
            // its output or input to the stage as a file name. The body is a
            // command the caller walks, so the word it fills is a value this
            // peel drops rather than a word of the command line.
            '<' | '>' if chars.get(i + 1) == Some(&'(') => {
                let end = paren_end(&chars, i + 1)?;
                push_marker(&mut replaced, CAPTURE_MARKER, false);
                i = end;
            }
            c => {
                replaced.push(c);
                i += 1;
            }
        }
    }
    Some(replaced)
}

/// The marker standing in for a substitution whose output is a value, glued to
/// the assignment name it fills.
const CAPTURE_MARKER: &str = "CAPTURE";

/// The marker standing in for a substitution whose output the shell runs as a
/// command name.
const COMMAND_MARKER: &str = "CMD";

/// Write what stands in for the substitution at the dollar sign and return the
/// index just past it.
///
/// Arithmetic expansion computes a number, so a value stands in for it and the
/// word around it keeps its shape. A command substitution runs, so a marker
/// stands in for it. Which marker depends on what the shell does with the
/// output: a substitution that fills an assignment is a value, while one in
/// command position names the program and is resolved at run time. Inside a
/// quoted run the marker is written between two quotes of its own so it stays
/// a standalone word instead of fusing into the word it split.
fn substitution_marker(
    replaced: &mut String,
    chars: &[char],
    dollar: usize,
    quoted: bool,
) -> Option<usize> {
    let end = paren_end(chars, dollar + 1)?;
    if chars.get(dollar + 2) == Some(&'(') {
        replaced.push('0');
        return Some(end);
    }
    let marker = if fills_assignment(replaced) {
        CAPTURE_MARKER
    } else {
        COMMAND_MARKER
    };
    push_marker(replaced, marker, quoted);
    Some(end)
}

/// Write a marker where a construct stood, spaced apart from the words around
/// it. Inside a quoted run the marker carries its own quotes, so it stays a
/// standalone word instead of fusing into the word the construct split.
fn push_marker(replaced: &mut String, marker: &str, quoted: bool) {
    if quoted {
        replaced.push_str("\" ");
        replaced.push_str(marker);
        replaced.push_str(" \"");
    } else {
        replaced.push(' ');
        replaced.push_str(marker);
        replaced.push(' ');
    }
}

/// Whether the text just written ends in an assignment the substitution fills,
/// so its output is a value. The name must be an identifier at the start of
/// its word: an equals sign inside an argument (a query parameter, an option
/// value) is not an assignment, and reading one as an assignment would let a
/// substitution rewrite that argument unseen.
fn fills_assignment(replaced: &str) -> bool {
    let start = replaced.rfind(char::is_whitespace).map_or(0, |at| at + 1);
    let token = replaced[start..].trim_end_matches('"');
    token.strip_suffix('=').is_some_and(is_identifier)
}

/// Peel one clause's shell constructs, returning the atomic program text
/// the classifiers should judge. Returns None when the clause holds a
/// construct this unwrapper cannot prove benign — a backquote, a
/// substitution in command position, an assignment that rebinds a request
/// setting — so the caller holds the clause rather than guessing.
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
    let mut in_loop_list = false;
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
        // A substitution filling an assignment is a value: the body is judged
        // by the caller's recursion, and the stage around it is not a command.
        if bare == CAPTURE_MARKER {
            continue;
        }
        // A substitution in command position names the program, and the shell
        // decides which program runs from the text it produces. The body is
        // judged by the caller, but the stage itself is not resolvable, so the
        // clause refuses rather than reading the next word as the program. A
        // loop header's marker is a generator the caller walks, so it peels
        // with the rest of the list.
        if bare == COMMAND_MARKER {
            if in_loop_list {
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
                in_loop_list = true;
                continue;
            }
            _ if in_loop_head => continue,
            // The words after in are the values the loop iterates, not a
            // program. They peel with the header; the generators among them
            // were already replaced by markers and are walked by the caller.
            _ if in_loop_list => continue,
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

/// Replace each substitution with a marker word, so a stage's own words stay
/// apart from the command spliced in the middle of them.
pub(crate) fn mark_substitutions(clause: &str) -> Option<String> {
    replace_substitutions_with_marker(clause)
}

/// Whether the clause's program is a substitution the shell resolves at run
/// time. The substitution's body is walked by the caller, but the program name
/// itself is text the scan never sees, so no stage-level check can vouch for
/// what runs.
pub(crate) fn leading_command_substitution(clause: &str) -> bool {
    // A backquote substitution is the older form this parser does not track,
    // so the clause head is unreadable in the same way.
    let Some(replaced) = replace_substitutions_with_marker(clause) else {
        return true;
    };
    for word in split_words_quote_aware(&replaced) {
        let bare = strip_word_quotes(&word);
        // The same assignment rule unwrap_clause applies: a plain NAME=value
        // precedes the program without being it. An assignment that rebinds a
        // request or loader setting leaves the head unreadable, which is the
        // refusal path unwrap_clause takes.
        if let Some(eq) = bare.find('=')
            && is_identifier(&bare[..eq])
            && !bare[eq + 1..].contains(' ')
        {
            if REFUSED_ENV_NAMES.contains(&&bare[..eq]) {
                return true;
            }
            continue;
        }
        let keyword = bare.trim_matches(';');
        if keyword.is_empty()
            || matches!(
                keyword,
                "!" | "{"
                    | "}"
                    | ";;"
                    | "case"
                    | "do"
                    | "done"
                    | "elif"
                    | "else"
                    | "esac"
                    | "fi"
                    | "for"
                    | "if"
                    | "in"
                    | "then"
                    | "until"
                    | "while"
                    | "break"
                    | "continue"
            )
        {
            continue;
        }
        return bare == COMMAND_MARKER;
    }
    false
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
                    if chars[i] == '`' {
                        return None;
                    }
                    // A substitution runs inside double quotes too.
                    if chars[i] == '$' && chars.get(i + 1) == Some(&'(') {
                        i = collect_substitution(&chars, i, &mut bodies)?;
                        continue;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            '`' => return None,
            '$' if chars.get(i + 1) == Some(&'(') => {
                i = collect_substitution(&chars, i, &mut bodies)?;
                continue;
            }
            // A process substitution runs in a subshell before the stage
            // reads its result.
            '<' | '>' if chars.get(i + 1) == Some(&'(') => {
                i = collect_process_substitution(&chars, i + 1, &mut bodies)?;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    Some(bodies)
}

/// Read the text between balanced parentheses, starting at the open one.
/// Quote-aware: a paren inside quotes is text, not nesting, so a body that
/// carries Python calls with their own parens keeps them. Returns None on
/// unbalanced parentheses: a malformed clause is refused, not guessed.
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

/// Collect the command bodies the substitution at the dollar sign runs and
/// return the index just past it.
///
/// Arithmetic expansion is not a command: it computes a number, so only a
/// command substitution nested inside it can run anything, and that body is
/// what comes back.
fn collect_substitution(chars: &[char], dollar: usize, bodies: &mut Vec<String>) -> Option<usize> {
    if chars.get(dollar + 2) == Some(&'(') {
        let (end, mut inner) = arithmetic_bodies(chars, dollar + 1)?;
        bodies.append(&mut inner);
        return Some(end);
    }
    bodies.push(read_paren_body(chars, dollar + 1)?);
    paren_end(chars, dollar + 1)
}

/// Collect the body of a process substitution whose opening paren sits at
/// open, and return the index just past its closing paren.
fn collect_process_substitution(
    chars: &[char],
    open: usize,
    bodies: &mut Vec<String>,
) -> Option<usize> {
    bodies.push(read_paren_body(chars, open)?);
    paren_end(chars, open)
}

/// Whether the clause runs a command in a process substitution.
///
/// The body of one is a command the caller walks, but the word the stage
/// receives is formed by the shell at run time, so a destination derived from
/// this clause would name a command the user never wrote.
pub(crate) fn has_process_substitution(clause: &str) -> bool {
    let chars: Vec<char> = clause.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    i += if chars[i] == '\\' { 2 } else { 1 };
                }
                i += 1;
            }
            '<' | '>' if chars.get(i + 1) == Some(&'(') => return true,
            _ => i += 1,
        }
    }
    false
}

/// The command substitutions inside an arithmetic expansion, and the index
/// just past the expansion.
fn arithmetic_bodies(chars: &[char], open: usize) -> Option<(usize, Vec<String>)> {
    let end = paren_end(chars, open)?;
    let mut bodies = Vec::new();
    let mut i = open + 1;
    while i < end {
        if chars[i] == '$' && chars.get(i + 1) == Some(&'(') {
            i = collect_substitution(chars, i, &mut bodies)?;
            continue;
        }
        i += 1;
    }
    Some((end, bodies))
}

/// The index just past balanced parentheses, starting at the open one, or
/// None when they never close. Quote-aware to match read_paren_body, so the
/// end both compute is the same index.
fn paren_end(chars: &[char], open: usize) -> Option<usize> {
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
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split on whitespace outside quotes. A word keeps its quotes so the
/// caller can strip them deliberately.
///
/// A backslash escapes the character after it outside single quotes, so an
/// escaped quote is a literal character rather than the end of a quoted run.
/// Reading it as a boundary split a one-liner whose quoted string contained
/// its own quotes, leaving the argument list describing words the shell never
/// passes.
pub(crate) fn split_words_quote_aware(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if !in_single => {
                escaped = true;
                cur.push(c);
            }
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

/// One word as the shell hands it to a program: quoting and escapes are
/// resolved, so a quoted subcommand reads as the bare word it runs.
pub(crate) fn decode_word(word: &str) -> String {
    let chars: Vec<char> = word.chars().collect();
    let mut out = String::with_capacity(word.len());
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
        if c == '\\' && !in_single && i + 1 < chars.len() {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Strip one layer of surrounding quotes from a word.
pub(crate) fn strip_word_quotes(word: &str) -> String {
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
