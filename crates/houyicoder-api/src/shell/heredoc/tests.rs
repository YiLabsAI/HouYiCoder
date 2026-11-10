//! Tests for the heredoc body walk.

use super::{heredoc_starts, strip_quoted_heredoc_bodies};

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
    assert!(out.contains("_HEREDOC_BODY_"), "body replaced: {out}");
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

/// The operator reports whether the shell will expand its body: a quoted or
/// escaped delimiter makes it literal, an unquoted one does not, and the
/// tab-stripping form is named.
#[test]
fn test_starts_report_quote_flag() {
    let starts = heredoc_starts("python3 - <<'EOF'");
    assert_eq!(starts.len(), 1);
    assert!(starts[0].quoted);
    assert_eq!(starts[0].delim, "EOF");
    let starts = heredoc_starts("cat <<-EOF");
    assert!(starts[0].tab_strip && !starts[0].quoted);
    assert!(heredoc_starts("echo $((1 << 2))").is_empty());
}
