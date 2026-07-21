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

/// Split a compound command into its top-level segments on and/or, semicolon,
/// and pipe. A bare pipe counts as a segment boundary (each stage of a pipeline
/// is a separate attestable unit). Empty segments are dropped.
pub fn split_compound(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // and/or operators: double ampersand or double pipe.
        if (c == '&' || c == '|') && i + 1 < chars.len() && chars[i + 1] == c {
            push_seg(&mut out, &cur);
            cur.clear();
            i += 2;
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

fn push_seg(out: &mut Vec<String>, s: &str) {
    let t = s.trim();
    if !t.is_empty() {
        out.push(t.into());
    }
}

/// Strip the redirect forms that do NOT touch a real file, so the
/// attestability scan only escalates on a redirect to a path. The
/// safe-redirect strip covers three forms: 2>&1 (stderr to stdout), an
/// optional source fd then > then /dev/null (discard stdout), and < then
/// /dev/null (discard stdin). The trailing boundary is mandatory — a
/// redirect to /dev/nullo must NOT match /dev/null as a prefix, else the
/// strip would hide a real file write and the redirect check would pass.
/// The expression here is a hand-rolled char scan.
fn strip_safe_redirects(content: &str) -> String {
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
/// form matches or the trailing boundary (whitespace or end-of-input) fails.
/// NOT quote-aware: a redirect inside a quoted string is rare data, an
/// acceptable approximation.
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

/// The trailing boundary a safe redirect must satisfy: followed by whitespace
/// or end-of-input. This is the detail that prevents a prefix match on
/// /dev/nullo from stripping the redirect and hiding a file write.
fn is_boundary(next: Option<char>) -> bool {
    matches!(next, None | Some(' ' | '\t' | '\n' | '\r'))
}

/// Whether a single command segment is structurally attestable: free of file
/// redirects, command substitution, heredoc, and process substitution outside
/// quotes. A redirect or substitution inside quotes is data, not an operator.
///
/// Safe redirect forms (2>&1, > /dev/null, < /dev/null) are stripped first so
/// cargo test 2>&1 reads as attestable while cargo test > log.txt escalates.
pub fn is_attestable(segment: &str) -> bool {
    let stripped = strip_safe_redirects(segment);
    let mut scan = QuoteScan::new(&stripped);
    while let Some(c) = scan.next() {
        if scan.in_quote() {
            continue;
        }
        if c == '>' || c == '<' {
            return false;
        }
        if c == '`' {
            return false;
        }
        // Command substitution: dollar plus open paren.
        if c == '$' && scan.peek_next() == Some('(') {
            return false;
        }
    }
    true
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

    fn in_quote(&self) -> bool {
        self.in_single || self.in_double
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_simple_segments() {
        assert_eq!(split_compound("ls && echo hi"), vec!["ls", "echo hi"]);
    }

    #[test]
    fn test_split_or_pipe() {
        assert_eq!(split_compound("a || b"), vec!["a", "b"]);
    }

    #[test]
    fn test_split_semicolon() {
        assert_eq!(split_compound("a; b ;c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_pipe() {
        assert_eq!(split_compound("a | b | c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_split_drops_empty() {
        assert_eq!(split_compound("a && && b"), vec!["a", "b"]);
    }

    #[test]
    fn test_attestable_simple_command() {
        assert!(is_attestable("ls -la"));
        assert!(is_attestable("grep foo bar.txt"));
    }

    #[test]
    fn test_quoted_redirect_attestable() {
        // An angle bracket inside quotes is data, not a redirect.
        assert!(is_attestable("echo 'a > b'"));
        assert!(is_attestable(r#"echo "a > b""#));
    }

    #[test]
    fn test_unattestable_redirect_out() {
        assert!(!is_attestable("echo hi > /tmp/x"));
    }

    #[test]
    fn test_unattestable_redirect_append() {
        assert!(!is_attestable("echo hi >> /tmp/x"));
    }

    #[test]
    fn test_input_redirect_unattestable() {
        assert!(!is_attestable("cat < /etc/passwd"));
    }

    #[test]
    fn test_attestable_fd_redirect() {
        // stderr to stdout never writes a file, so it is safe. Other fd
        // combinations (1>&2, >&2) are NOT in the safe set — only 2>&1 is
        // stripped, the rest escalate.
        assert!(is_attestable("cargo test 2>&1"));
        assert!(!is_attestable("cargo test 1>&2"));
        assert!(!is_attestable("make check >&2"));
    }

    #[test]
    fn test_attestable_dev_null_discard() {
        assert!(is_attestable("cargo test > /dev/null"));
        assert!(
            is_attestable("cargo test 2>/dev/null") || is_attestable("cargo test 2> /dev/null")
        );
        assert!(is_attestable("grep foo bar < /dev/null"));
    }

    #[test]
    fn test_unattestable_dev_null_prefix() {
        // The trailing boundary is load-bearing: > /dev/nullo must NOT match
        // /dev/null as a prefix (else the strip hides a real file write).
        assert!(!is_attestable("echo hi > /dev/nullo"));
    }

    #[test]
    fn test_unattestable_heredoc() {
        assert!(!is_attestable("cat <<EOF"));
    }

    #[test]
    fn test_unattestable_cmd_subst_dollar() {
        assert!(!is_attestable("echo $(whoami)"));
    }

    #[test]
    fn test_unattestable_cmd_subst_backtick() {
        assert!(!is_attestable("echo `whoami`"));
    }

    #[test]
    fn test_compound_safe_all_attestable() {
        assert!(compound_safe(&["ls", "grep foo"]));
    }

    #[test]
    fn test_compound_safe_one_unattestable() {
        assert!(!compound_safe(&["ls", "echo hi > /tmp/x"]));
    }

    #[test]
    fn test_empty_compound_unsafe() {
        assert!(!compound_safe(&[]));
    }

    #[test]
    fn test_compound_safe_mixed_chain() {
        let segs = split_compound("ls && rm -rf /tmp/x > /tmp/log");
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!compound_safe(&refs));
    }
}
