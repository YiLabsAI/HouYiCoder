//! Quoted-heredoc body stripping for the safety scan. A cat <<'EOF' body is
//! literal text for cat, not a command; the destructive-command + protected
//! path scans must not flag a destructive-looking line inside it. This
//! replaces the body (lines between the operator and the closing delimiter)
//! with a placeholder before the scan. Unquoted heredocs (<<EOF) keep their
//! bodies — bash expands $() in them, so a real command could hide there.
//!
//! A reduced implementation: it covers the common
//! single/double/escaped-delimiter forms (and the <<- tab-stripped forms),
//! not every edge case (bit-shift disambiguation via the (( before <<,
//! nested heredocs, $' / backtick bail-outs). The
//! quote requirement on the delimiter avoids matching a bit-shift like
//! a << 2 (no quote follows).

/// A heredoc start found on a line: the delimiter to match on its own line
/// later, and whether the operator was the tab-stripping <<- form.
struct Start {
    delim: String,
    tab_strip: bool,
}

/// Replace the bodies of quoted/escaped heredocs with a placeholder so the
/// safety scan does not see their literal text. Unquoted heredocs keep their
/// bodies. The operator line and the closing delimiter line are preserved
/// (only the body lines between them are replaced).
#[expect(
    clippy::while_let_on_iterator,
    reason = "inner loop consumes same iterator"
)] // inner loop consumes the same iterator
pub(crate) fn strip_quoted_heredoc_bodies(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        out.push_str(line);
        out.push('\n');
        // A line can start several heredocs (cat <<'A' <<'B'); consume each
        // one's body in order. The shared iterator advances past body lines
        // so the outer loop does not re-process them.
        for start in quoted_heredoc_starts(line) {
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
                out.push_str("<heredoc-body>");
                out.push('\n');
            }
        }
    }
    out
}

/// Find quoted/escaped heredoc starts on a line: <<[-]?('(\w+)'|"(\w+)"|\\(\w+)).
/// Bare <<EOF (unquoted) is NOT returned — its body stays visible to the
/// scan because bash expands it. The quote requirement also avoids matching
/// a bit-shift like a << 2.
fn quoted_heredoc_starts(line: &str) -> Vec<Start> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 1 < n {
        // Skip the here-string form <<< (not a heredoc body to strip).
        if chars[i] == '<' && chars[i + 1] == '<' && chars.get(i + 2) == Some(&'<') {
            i += 3;
            continue;
        }
        if chars[i] == '<' && chars[i + 1] == '<' {
            let mut j = i + 2;
            let tab_strip = if j < n && chars[j] == '-' {
                j += 1;
                true
            } else {
                false
            };
            while j < n && chars[j] == ' ' {
                j += 1;
            }
            let delim = read_quoted_delim(&chars, &mut j);
            if let Some(d) = delim {
                starts.push(Start {
                    delim: d,
                    tab_strip,
                });
                i = j;
                continue;
            }
        }
        i += 1;
    }
    starts
}

/// Read a quoted or escaped delimiter at chars[*j..]. A single- or
/// double-quoted word, or a backslash-escaped word. Returns None for a bare
/// (unquoted) word so the caller leaves its body visible.
fn read_quoted_delim(chars: &[char], j: &mut usize) -> Option<String> {
    let n = chars.len();
    if *j >= n {
        return None;
    }
    if chars[*j] == '\'' || chars[*j] == '"' {
        let quote = chars[*j];
        *j += 1;
        let start = *j;
        while *j < n && chars[*j] != quote {
            *j += 1;
        }
        if *j < n {
            let d: String = chars[start..*j].iter().collect();
            *j += 1; // skip the closing quote
            Some(d)
        } else {
            None
        }
    } else if chars[*j] == '\\' {
        *j += 1;
        let start = *j;
        while *j < n && (chars[*j].is_alphanumeric() || chars[*j] == '_') {
            *j += 1;
        }
        if *j > start {
            Some(chars[start..*j].iter().collect())
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::strip_quoted_heredoc_bodies;

    /// A quoted heredoc body is replaced; the operator + delimiter lines stay.
    #[test]
    fn test_single_quoted_body_stripped() {
        let cmd = "cat <<'EOF'\nrm -rf .git/\nEOF";
        let out = strip_quoted_heredoc_bodies(cmd);
        assert!(out.contains("cat <<'EOF'"), "operator kept: {out}");
        assert!(out.contains("EOF"), "delimiter kept: {out}");
        assert!(
            !out.contains("rm -rf"),
            "body literal must not reach the scan: {out}"
        );
        assert!(out.contains("<heredoc-body>"), "body replaced: {out}");
    }

    /// An unquoted heredoc keeps its body — bash expands it, so a real command
    /// could hide there.
    #[test]
    fn test_unquoted_body_kept() {
        let cmd = "cat <<EOF\nrm -rf x\nEOF";
        let out = strip_quoted_heredoc_bodies(cmd);
        assert!(
            out.contains("rm -rf x"),
            "unquoted body must stay visible: {out}"
        );
    }

    /// A bit-shift a << 2 is not a heredoc start (no quote follows the <<).
    #[test]
    fn test_bit_shift_not_heredoc() {
        let cmd = "echo $((1 << 2))";
        let out = strip_quoted_heredoc_bodies(cmd);
        assert_eq!(out, "echo $((1 << 2))\n");
    }

    /// A double-quoted delimiter + the escaped-delimiter form are stripped too.
    #[test]
    fn test_double_escaped_delim_stripped() {
        let cmd = "cat <<\"END\"\nbad line\nEND";
        assert!(
            !strip_quoted_heredoc_bodies(cmd).contains("bad line"),
            "double-quoted body stripped"
        );
        let cmd = "cat <<\\X\nbad line 2\nX";
        assert!(
            !strip_quoted_heredoc_bodies(cmd).contains("bad line 2"),
            "escaped-delimiter body stripped"
        );
    }
}
