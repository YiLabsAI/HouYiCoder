//! Heredoc body handling for the command scans.
//!
//! A heredoc body is text the shell hands to the program reading standard
//! input, never a line of the command. The scan therefore treats it
//! separately: a quoted body is literal text to the shell and code to the
//! program, while an unquoted body is expanded by the shell first. This walk
//! reports each body with its operator line and whether it was quoted, and
//! replaces the bodies a caller asks it to hide.

/// A heredoc operator found on a line.
pub struct HeredocStart {
    /// The delimiter to match on its own line later.
    pub delim: String,
    /// Whether the operator was the tab-stripping form, which ignores leading
    /// tabs when it compares the closing delimiter.
    pub tab_strip: bool,
    /// Whether the delimiter was quoted or escaped, making the body literal
    /// text the shell never expands.
    pub quoted: bool,
}

/// Replace the bodies of quoted and escaped heredocs with a placeholder so a
/// safety scan does not see their literal text. An unquoted body keeps its
/// text because the shell expands it.
pub fn strip_quoted_heredoc_bodies(content: &str) -> String {
    walk_heredoc_bodies(content, false, |_, _, _, _| {})
}

/// Replace every heredoc body with a placeholder, quoted or not, so a caller
/// reading the command structure around the bodies does not read the bodies as
/// commands.
pub fn strip_heredoc_bodies(content: &str) -> String {
    walk_heredoc_bodies(content, true, |_, _, _, _| {})
}

/// The bodies of the unquoted heredocs in a command. The shell expands one
/// before the reading program sees the text, so a substitution inside it runs
/// even though the body itself is not a command line.
pub fn unquoted_heredoc_bodies(content: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    walk_heredoc_bodies(content, true, |_, _, quoted, line| {
        if !quoted {
            bodies.push(line.to_string());
        }
    });
    bodies
}

/// The bodies of the quoted and escaped heredocs in a command, in the order
/// they appear. A quoted body is literal text to the shell but still code to
/// the program reading it on standard input, so a caller judging what a script
/// writes has to see it even though the safety scan does not.
pub fn quoted_heredoc_bodies(content: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    walk_quoted_heredoc_bodies(content, |_, _, line| bodies.push(line.to_string()));
    bodies
}

/// Walk the quoted and escaped heredoc bodies, handing each line to the caller
/// with the line its operator sits on and the ordinal of that operator among
/// the heredoc starts on the line.
pub fn walk_quoted_heredoc_bodies(
    content: &str,
    mut on_body: impl FnMut(&str, usize, &str),
) -> String {
    walk_heredoc_bodies(content, false, |operator_line, ordinal, quoted, line| {
        if quoted {
            on_body(operator_line, ordinal, line);
        }
    })
}

/// Walk every heredoc body, handing each line to the caller with the operator
/// line, the ordinal of the operator among the starts on that line, whether
/// its delimiter was quoted, and the body line. Bodies are replaced with a
/// placeholder when they are quoted, or always when strip_all is set.
///
/// The operator line and the closing delimiter line stay in the returned text.
#[expect(
    clippy::while_let_on_iterator,
    reason = "inner loop consumes same iterator"
)]
pub fn walk_heredoc_bodies(
    content: &str,
    strip_all: bool,
    mut on_body: impl FnMut(&str, usize, bool, &str),
) -> String {
    let mut out = String::with_capacity(content.len());
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        out.push_str(line);
        out.push('\n');
        // A line can start several heredocs (cat <<'A' <<'B'); consume each
        // one's body in order. The shared iterator advances past body lines
        // so the outer loop does not re-process them.
        for (ordinal, start) in heredoc_starts(line).into_iter().enumerate() {
            while let Some(body_line) = lines.next() {
                let cmp = if start.tab_strip {
                    body_line.trim_start_matches('\t')
                } else {
                    body_line
                };
                if cmp == start.delim {
                    out.push_str(body_line);
                    out.push('\n');
                    break;
                }
                on_body(line, ordinal, start.quoted, body_line);
                if strip_all || start.quoted {
                    out.push_str("_HEREDOC_BODY_");
                } else {
                    out.push_str(body_line);
                }
                out.push('\n');
            }
        }
    }
    out
}

/// Find every heredoc operator on a line, quoted or unquoted.
pub fn heredoc_starts(line: &str) -> Vec<HeredocStart> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 1 < n {
        // Skip the here-string form <<< (not a heredoc body).
        if chars[i] == '<' && chars[i + 1] == '<' && chars.get(i + 2) == Some(&'<') {
            i += 3;
            continue;
        }
        if chars[i] == '<' && chars[i + 1] == '<' {
            let mut j = i + 2;
            let tab_strip = chars.get(j) == Some(&'-');
            if tab_strip {
                j += 1;
            }
            // A delimiter may stand a space away from the operator, as bash
            // accepts. Inside an arithmetic expansion a spaced operator is a
            // shift, and reading it as a heredoc would consume the rest of the
            // input as a body that never closes.
            let mut spaced = j;
            while chars.get(spaced) == Some(&' ') {
                spaced += 1;
            }
            let quoted_ahead = matches!(chars.get(spaced), Some('\'' | '"' | '\\'));
            let bare_ahead = chars.get(spaced).is_some_and(|c| c.is_ascii_alphabetic())
                && !inside_arithmetic(&chars[..i]);
            if quoted_ahead || bare_ahead {
                j = spaced;
            }
            if let Some((delim, quoted)) = read_delim(&chars, &mut j) {
                starts.push(HeredocStart {
                    delim,
                    tab_strip,
                    quoted,
                });
                i = j;
                continue;
            }
        }
        i += 1;
    }
    starts
}

/// Whether the chars end inside an arithmetic expansion, where a spaced shift
/// operator looks like a heredoc operator.
fn inside_arithmetic(chars: &[char]) -> bool {
    let mut depth = 0usize;
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i] == '$' && chars[i + 1] == '(' && chars.get(i + 2) == Some(&'(') {
            depth += 1;
            i += 3;
            continue;
        }
        if depth > 0 && chars[i] == ')' && chars[i + 1] == ')' {
            depth -= 1;
            i += 2;
            continue;
        }
        i += 1;
    }
    depth > 0
}

/// Read a delimiter at chars[*j..]: a quoted word, a backslash-escaped word,
/// or a bare identifier. A bare delimiter must start with a letter so that an
/// arithmetic shift is not read as one; it is unquoted, so the shell expands
/// the body.
fn read_delim(chars: &[char], j: &mut usize) -> Option<(String, bool)> {
    let n = chars.len();
    let first = *chars.get(*j)?;
    if first == '\'' || first == '"' {
        let quote = first;
        *j += 1;
        let start = *j;
        while *j < n && chars[*j] != quote {
            *j += 1;
        }
        if *j >= n {
            return None;
        }
        let delim: String = chars[start..*j].iter().collect();
        *j += 1;
        return Some((delim, true));
    }
    if first == '\\' {
        *j += 1;
        let start = *j;
        while *j < n && (chars[*j].is_alphanumeric() || chars[*j] == '_') {
            *j += 1;
        }
        if *j > start {
            return Some((chars[start..*j].iter().collect(), true));
        }
        return None;
    }
    if first.is_ascii_alphabetic() {
        let start = *j;
        while *j < n && (chars[*j].is_ascii_alphanumeric() || chars[*j] == '_') {
            *j += 1;
        }
        return Some((chars[start..*j].iter().collect(), false));
    }
    None
}

#[cfg(test)]
mod tests;
