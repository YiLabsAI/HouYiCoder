//! Tests for the shell command translation.

use super::{Shell, ShellCommand};

/// The text a Git Bash host runs for the command.
fn git_bash(command: &str) -> String {
    ShellCommand::for_shell(command, Shell::GitBash).into_actual()
}

/// A declared null device is the one redirect target that moves to the POSIX
/// spelling. The operator, its file descriptor, and the surrounding text are
/// the caller's and stay exactly as written.
#[test]
fn test_null_redirect_rewritten() {
    for (command, expected) in [
        ("echo hi >nul", "echo hi >/dev/null"),
        ("echo hi >nul 2>&1", "echo hi >/dev/null 2>&1"),
        ("cmd 2>nul", "cmd 2>/dev/null"),
        ("cmd &>nul", "cmd &>/dev/null"),
        ("cmd >>nul", "cmd >>/dev/null"),
        ("cmd > nul", "cmd > /dev/null"),
        ("cmd >NUL", "cmd >/dev/null"),
        ("cmd >Nul", "cmd >/dev/null"),
        ("echo a >nul;", "echo a >/dev/null;"),
        ("(echo a >nul)", "(echo a >/dev/null)"),
        ("echo $(run >nul)", "echo $(run >/dev/null)"),
        ("find . | xargs cat >nul", "find . | xargs cat >/dev/null"),
        ("echo a >nul\r\n", "echo a >/dev/null\r\n"),
        ("echo a >nul>nul", "echo a >/dev/null>/dev/null"),
    ] {
        assert_eq!(git_bash(command), expected, "for {command:?}");
    }
}

/// The same word where it is not a redirect target is data or a file name and
/// keeps its spelling: an operand, a longer word that starts with it, a quoted
/// target, an escaped operator, or a comment.
#[test]
fn test_null_operand_untouched() {
    for command in [
        "echo nul",
        "grep -n nul notes.txt",
        "mv nul elsewhere",
        "echo x >nul.txt",
        "echo x > nul.log",
        "echo x >'nul'",
        "echo x >\"nul\"",
        "echo \\>nul",
        "echo hi # >nul",
        "cmd 2>&1",
        "echo > /dev/null",
    ] {
        assert_eq!(git_bash(command), command, "for {command:?}");
    }
}

/// Text inside quotes is program data, and a redirection operator spelled
/// there does not reach the shell.
#[test]
fn test_quoted_text_untouched() {
    for (command, expected) in [
        ("echo '>nul'", "echo '>nul'"),
        ("echo \">nul\"", "echo \">nul\""),
        ("echo 'a >nul b' >nul", "echo 'a >nul b' >/dev/null"),
        ("echo \"a >nul b\" >nul", "echo \"a >nul b\" >/dev/null"),
    ] {
        assert_eq!(git_bash(command), expected, "for {command:?}");
    }
}

/// A heredoc body is the input of a program, not a command line, so a
/// redirection inside it reaches the program unchanged.
#[test]
fn test_heredoc_body_untouched() {
    for command in [
        "cat <<EOF\n>nul\nEOF\n",
        "cat <<'EOF'\n>nul\nEOF\n",
        "cat <<-EOF\n\t>nul\n\tEOF\n",
        "cat <<EOF\nkeep >nul keep\nEOF\n",
    ] {
        assert_eq!(git_bash(command), command, "for {command:?}");
    }
}

/// The start line of a heredoc still carries the command's own redirections,
/// which the shell applies, so a target there is rewritten while its body
/// stays untouched.
#[test]
fn test_heredoc_line_rewritten() {
    assert_eq!(
        git_bash("cat <<EOF >nul\nbody >nul\nEOF\n"),
        "cat <<EOF >/dev/null\nbody >nul\nEOF\n"
    );
}

/// Only a shell whose null device is the POSIX one translates the target: a
/// unix host reads nul as a file name, and the command interpreter owns its
/// own device vocabulary.
#[test]
fn test_other_shells_untouched() {
    for shell in [Shell::Posix, Shell::Cmd] {
        let command = "echo x >nul";
        let translated = ShellCommand::for_shell(command, shell);
        assert!(!translated.changed(), "{shell:?}");
        assert_eq!(translated.into_actual(), command, "{shell:?}");
    }
}

/// A translation is reported, so a caller can tell a command the host runs as
/// written from one it runs differently.
#[test]
fn test_changed_reports_translation() {
    assert!(
        ShellCommand::for_shell("echo x >nul", Shell::GitBash).changed(),
        "a moved target is a translation"
    );
    assert!(
        !ShellCommand::for_shell("echo x", Shell::GitBash).changed(),
        "an untouched command is not"
    );
}
