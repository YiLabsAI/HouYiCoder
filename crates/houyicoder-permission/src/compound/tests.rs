use super::*;
use crate::bash_always_allow_prefix;

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

/// The shell ends a command at a newline and at a single ampersand, so the
/// splitter must too: a segment carrying one would describe one program while
/// the shell runs two.
#[test]
fn test_split_newline_and_background() {
    assert_eq!(split_compound("ls\ncp a b"), vec!["ls", "cp a b"]);
    assert_eq!(split_compound("ls & touch x"), vec!["ls", "touch x"]);
    assert_eq!(split_compound("a;\nb\r\nc"), vec!["a", "b", "c"]);
}

/// A backslash continues the command on the next line, and an ampersand that
/// joins a redirect stays in the segment.
#[test]
fn test_split_keeps_joined_forms() {
    assert_eq!(split_compound("cargo \\\nbuild"), vec!["cargo build"]);
    assert_eq!(split_compound("cargo test 2>&1"), vec!["cargo test 2>&1"]);
    assert_eq!(
        split_compound("make check &> log"),
        vec!["make check &> log"]
    );
}

/// A heredoc body is input rather than a command list, so its lines are not
/// segments; the command after the delimiter line is.
#[test]
fn test_split_heredoc_body() {
    assert_eq!(
        split_compound("cat <<'EOF'\nrm -rf x\necho done\nEOF\nls"),
        vec!["cat <<'EOF'", "ls"]
    );
    assert_eq!(
        split_compound("cat <<-EOF\n\tbody\n\tEOF\necho done"),
        vec!["cat <<-EOF", "echo done"]
    );
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
    // Neither direction names a file: 2>&1 sends stderr to stdout and >&2
    // sends stdout to stderr. Both are plumbing a script emits, so both read
    // as attestable. A command that sends both streams to a file spells that
    // &> path, whose ampersand comes before the angle, and stays a write.
    assert!(is_attestable("cargo test 2>&1"));
    assert!(is_attestable("cargo test 1>&2"));
    assert!(is_attestable("make check >&2"));
    assert!(is_attestable("cargo test 1 >& 2"));
    assert!(is_attestable("cargo test 0>&2"));
    assert!(!is_attestable("make check &> build.log"));
}

#[test]
fn test_attestable_group_redirect() {
    // The redirect is the last thing before the group closes, so the closing
    // paren is the boundary it has to be recognised against.
    assert!(is_attestable("(git rev-parse --show-toplevel 2>/dev/null)"));
    let segs = split_compound("(git rev-parse --show-toplevel 2>/dev/null) | wc -l");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(
        compound_safe(&refs),
        "a grouped pipe reads as safe: {refs:?}"
    );
}

#[test]
fn test_attestable_before_separator() {
    // A separator ends the segment on the spot, so a redirect does not need a
    // space after it to be recognised.
    assert!(is_attestable("cargo test 2>/dev/null;"));
    assert!(is_attestable("cargo test 2>/dev/null|wc -l"));
}

#[test]
fn test_readonly_filter_tail() {
    // A survey command ends with a counting or sorting tail; none of these
    // write, so the chain stays read-only.
    for cmd in [
        "echo $(pwd) | sort",
        "echo $(pwd) | sort | uniq",
        "echo $(pwd) | cut -d/ -f1",
        "echo $(pwd) | tr a-z A-Z",
        "echo \"a b\" | sort file.txt",
        "echo $(whoami) | cut -d: -f1 | sort -u",
        "echo `whoami` | sort",
        "echo $(echo \"a)b\") | sort",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs), "read-only: {cmd}");
    }
}

#[test]
fn test_not_readonly_sort_output() {
    // sort names its destination as an argument, where the redirect scan
    // cannot see it, so the command itself has to be asked. Every spelling the
    // tool accepts is covered: the short flag alone and inside a cluster, the
    // long flag whole and abbreviated, and the two options that write or run
    // something else.
    for cmd in [
        "echo $(pwd) | sort -o out.txt",
        "echo $(pwd) | sort -oout.txt",
        "echo $(pwd) | sort -no out.txt",
        "echo $(pwd) | sort -ro out.txt",
        "echo $(pwd) | sort --output=out.txt",
        "echo $(pwd) | sort --output out.txt",
        "echo $(pwd) | sort --outp=out.txt",
        "echo $(pwd) | sort -T scratchdir",
        "echo $(pwd) | sort --compress-program=gzip",
        "echo $(pwd) | sort \"-o\" out.txt",
        "echo $(pwd) | sort '-no' out.txt",
        "echo $(pwd) | sort \\-o out.txt",
        "echo $(pwd) | sort $FLAG out.txt",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs), "writes a file: {cmd}");
    }
    // The same filter without a destination still reads.
    for cmd in [
        "echo $(pwd) | sort",
        "echo $(pwd) | sort -n",
        "echo $(pwd) | sort -k1,1",
        "echo $(pwd) | sort -- file.txt",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs), "reads only: {cmd}");
    }
}

#[test]
fn test_not_readonly_count_output() {
    // uniq takes its output file as a second positional operand, with no flag
    // to recognise, so the operand count is what decides.
    for cmd in ["cat f | uniq - out.txt", "cat f | uniq in.txt out.txt"] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs), "writes a file: {cmd}");
    }
    for cmd in ["cat f | uniq", "cat f | uniq -c", "cat f | uniq in.txt"] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs), "reads only: {cmd}");
    }
}

#[test]
fn test_quote_is_not_boundary() {
    // Quoting concatenates mid-word, so a quoted fragment after a redirect
    // names the file the concatenation spells. A quote must not end the match,
    // or the strip would hide a write to a path the check never sees.
    assert!(!is_attestable("echo x >/dev/null\"o\""));
    assert!(!is_attestable("echo x >/dev/null'o'"));
    assert!(!is_attestable("echo x 2>/dev/null\"o\""));
}

#[test]
fn test_attestable_dev_null_discard() {
    assert!(is_attestable("cargo test > /dev/null"));
    assert!(is_attestable("cargo test 2>/dev/null") || is_attestable("cargo test 2> /dev/null"));
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
fn test_attestable_quoted_heredoc() {
    // A quoted heredoc <<'DELIM' feeds literal text (no expansion);
    // the body is stripped by strip_quoted_heredoc_bodies. The <<
    // operator is not a file redirect, so the segment is attestable.
    assert!(is_attestable("ego-browser nodejs <<'EOF'"));
    assert!(is_attestable("cat <<\"EOF\""));
}

#[test]
fn test_quoted_heredoc_body_chars() {
    // A quoted heredoc with a body containing > or < chars: the body
    // is stripped to _HEREDOC_BODY_ so those chars do not affect
    // attestability.
    assert!(is_attestable(
        "ego-browser nodejs <<'EOF'\nconst x = a > b;\nEOF"
    ));
    assert!(is_attestable("cat <<'MARK'\nline with < and > chars\nMARK"));
}

#[test]
fn test_unquoted_heredoc_stays_unsafe() {
    assert!(!is_attestable("cat <<EOF\nbody\nEOF"));
}

#[test]
fn test_dash_heredoc_attestable() {
    assert!(is_attestable("cat <<-'EOF'\nbody\nEOF"));
}

#[test]
fn test_bash_prefix_heredoc() {
    assert_eq!(
        bash_always_allow_prefix("ego-browser nodejs <<'EOF'\nbody\nEOF"),
        Some("ego-browser".to_string())
    );
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
fn test_readonly_pipe_chain_subst() {
    // The user-reported case: a pipe chain of read-only commands with a
    // which-substitution and a stderr-to-dev-null redirect auto-allows.
    let cmd = "strings $(which ego-browser) 2>/dev/null | grep -i 'ego' | head -40";
    let segs = split_compound(cmd);
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(is_readonly_compound(&refs), "read-only chain should allow");
}

#[test]
fn test_readonly_single_subst() {
    // A single read-only command with a read-only substitution.
    let segs = split_compound("echo $(whoami)");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(is_readonly_compound(&refs));
}

#[test]
fn test_readonly_nested_subst() {
    // Substitution whose body is itself a compound of read-only commands.
    let segs = split_compound("head -5 $(grep foo bar | head -3)");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(is_readonly_compound(&refs));
}

/// A substitution nested deeper than the recursion limit fails closed: the
/// chain asks rather than being read as read-only on an unverified inner body.
#[test]
fn test_readonly_depth_limit() {
    let segs = split_compound("echo $(echo $(echo $(echo x)))");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(!is_readonly_compound(&refs), "too deep to attest");
}

#[test]
fn test_not_readonly_file_redirect() {
    // A file redirect to a real path writes; not read-only.
    let segs = split_compound("echo hi > /tmp/x");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(!is_readonly_compound(&refs));
}

#[test]
fn test_not_readonly_destructive_subst() {
    // A substitution body whose command is not in the read-only set
    // (rm here) is not read-only.
    let segs = split_compound("strings $(rm -rf /tmp)");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(!is_readonly_compound(&refs));
}

#[test]
fn test_not_readonly_network_subst() {
    // A network tool in a substitution is not read-only.
    let segs = split_compound("strings $(curl http://evil.com)");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(!is_readonly_compound(&refs));
}

#[test]
fn test_not_readonly_write_command() {
    // A write/exec command as the first token is not read-only.
    let segs = split_compound("sed -i 's/x/y/' file | grep y");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(!is_readonly_compound(&refs));
}

#[test]
fn test_unbalanced_paren_fails_closed() {
    // An unbalanced command substitution must not be treated as read-only.
    let segs = split_compound("echo $(whoami");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(!is_readonly_compound(&refs));
}

/// A quote sequence this scan cannot follow, such as an escape inside an
/// ANSI-C string, leaves the splitter and the shell disagreeing about where a
/// word ends, so a following command reads as an argument of the first one.
/// Nothing in that shape is attested as safe, read-only, or free of effect.
#[test]
fn test_unbalanced_quote_asks() {
    let cmd = r"ls $'a\'b' ; cp secret /tmp/x";
    let segments = split_compound(cmd);
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    assert!(!compound_safe(&refs), "{segments:?}");
    assert!(!is_readonly_compound(&refs), "{segments:?}");
    assert!(!is_effect_safe(cmd, true));
}

#[test]
fn test_split_quoted_pipe_nosplit() {
    // A pipe inside quotes is data, not a segment boundary.
    let segs = split_compound("echo 'a|b' | cat");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert_eq!(refs.len(), 2, "quoted pipe must not split: {refs:?}");
    assert!(
        refs.iter()
            .all(|s| !s.contains("'a|b'") || s.contains("echo"))
    );
}

#[test]
fn test_not_readonly_passthrough_command() {
    // command is a passthrough builtin: it runs the wrapped command, so
    // it is not in the read-only set (would let command-curl through).
    let segs = split_compound("command whoami");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(!is_readonly_compound(&refs));
}

#[test]
fn test_not_readonly_date_positional() {
    // date has a dangerous positional form (sets the clock); excluded.
    let segs = split_compound("date 01010000");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(!is_readonly_compound(&refs));
}

#[test]
fn test_not_readonly_hostname_positional() {
    // hostname has a dangerous positional form (sets the host); excluded.
    let segs = split_compound("hostname foo");
    let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
    assert!(!is_readonly_compound(&refs));
}

#[test]
fn test_readonly_across_leading_cd() {
    // A move into one literal directory places the reads that follow it and
    // has no effect of its own, so a survey that starts with one still reads.
    for cmd in [
        "cd /work/repo && head README.md",
        "cd /work/repo && grep -rn todo . | wc -l",
        "cd ~ && ls",
        "cd /a; cat b",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs), "read-only: {cmd}");
    }
}

#[test]
fn test_not_readonly_cd_destination() {
    // The destination must be a literal token: an expansion or a redirect
    // makes it something the check cannot see, and a move placed after a read
    // is a second command rather than the setup for the first.
    for cmd in [
        "cd $(mktemp -d) && head x",
        "cd $DIR && head x",
        "cd /a > log && head x",
        "head x && cd /a",
        "cd /a && rm x",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs), "not read-only: {cmd}");
    }
}

#[test]
fn test_not_readonly_leading_assignment() {
    // A variable set for one command can redirect the repository directory
    // or the loader search path, so the argv no longer describes what runs.
    for cmd in [
        "GIT_DIR=/x git log",
        "LD_PRELOAD=/tmp/x.so cat y",
        "LC_ALL=C sort y",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs), "not read-only: {cmd}");
    }
}

#[test]
fn test_readonly_repository_commands() {
    for cmd in [
        "git status",
        "git log --oneline -n 5",
        "git diff --stat",
        "git show HEAD:file",
        "git rev-parse HEAD",
        "git hash-object file",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs), "read-only: {cmd}");
    }
}

#[test]
fn test_not_readonly_repository_writes() {
    for cmd in [
        "git hash-object -w file",
        "git diff --output=patch.diff",
        "git diff --textconv",
        "git log --ext-diff",
        "git log -o.claude/settings.json",
        "git config user.name",
        "git add .",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs), "not read-only: {cmd}");
    }
}

#[test]
fn test_readonly_search_commands() {
    for cmd in [
        "find . -name '*.rs'",
        "find . -type f -newer a.rs",
        "rg todo crates",
        "rg -n 'fn main' .",
        "diff a.rs b.rs",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs), "read-only: {cmd}");
    }
}

#[test]
fn test_not_readonly_search_writes() {
    // A search that deletes, runs a program, or writes a file is not a read,
    // whichever way the destination is spelled.
    for cmd in [
        "find . -delete",
        "find . -exec rm {} \\;",
        "find . -fprint out.txt",
        "rg --pre ./run.sh todo",
        "rg --pre=./run.sh todo",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs), "not read-only: {cmd}");
    }
}

#[test]
fn test_readonly_network_fetch() {
    for cmd in [
        "curl http://example.com",
        "curl -sS https://example.com/status.json",
        "curl --url http://example.com",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(is_readonly_compound(&refs), "read-only: {cmd}");
    }
}

#[test]
fn test_not_readonly_network_upload() {
    // Anything that writes a local file, sends a body, or names a non-http
    // operand is not a plain fetch.
    for cmd in [
        "curl -o out.html http://example.com",
        "curl -O http://example.com/a.txt",
        "curl --output=out.html http://example.com",
        "curl -d secret=1 http://example.com",
        "curl --data=secret=1 http://example.com",
        "curl -X POST http://example.com",
        "curl ftp://example.com",
        "curl help",
    ] {
        let segs = split_compound(cmd);
        let refs: Vec<&str> = segs.iter().map(|s| s.as_str()).collect();
        assert!(!is_readonly_compound(&refs), "not read-only: {cmd}");
    }
}

/// A protected path read with an argument that carries spaces inside quotes is
/// still one command with one argument. Splitting the quoted text on its
/// spaces left an unterminated quote for the word decoder, so a plain read of
/// a control file required approval every time.
#[test]
fn test_protected_read_quoted_arg() {
    for cmd in [
        "stat -f \"settings.json mtime: %Sm\" ~/.houyicoder/settings.json",
        "grep \"theme name\" .claude/settings.json",
        "shasum -a 256 .claude/settings.json",
        "sha256sum .houyicoder/permissions.json",
        "md5 .claude/settings.json",
        "jq \".theme.name\" .houyicoder/settings.json",
    ] {
        assert!(reads_only_protected(cmd), "reads only: {cmd}");
    }
}

/// The quoted argument must not become a way to hide a write.
#[test]
fn test_quoted_arg_hides_write() {
    for cmd in [
        "shasum \"settings.json\" .claude/settings.json > out.txt",
        "sort \"-o out\" .claude/settings.json",
    ] {
        assert!(!reads_only_protected(cmd), "asks: {cmd}");
    }
}

/// An angle bracket inside a quoted argument is data, not a redirect. A
/// quote-blind search for one refused a probe whose filter or format carried
/// it, so a plain read of a settings file needed approval.
#[test]
fn test_quoted_angle_is_data() {
    for cmd in [
        "jq '.count > 0' ~/.houyicoder/settings.json",
        "stat -f \"mtime -> %Sm\" ~/.houyicoder/settings.json",
    ] {
        assert!(reads_only_protected(cmd), "reads only: {cmd}");
    }
}

/// The fence refusal rests on a write the command performs, not on text it
/// carries: the same path in a commit message or in a string redirected
/// elsewhere belongs to commands that run.
#[test]
fn test_mention_is_not_write() {
    assert!(!writes_fence_denied_path(
        "git commit -m \"fix .houyicoder/settings.json\""
    ));
    assert!(!writes_fence_denied_path(
        "echo \"ok .claude/settings.json\" > /tmp/report.txt"
    ));
}

/// A redirect whose target is the control path, or a writing command naming
/// it, is a write and stays refused.
#[test]
fn test_control_write_detected() {
    for cmd in [
        "echo x > ~/.houyicoder/settings.json",
        "printf y | tee .houyicoder/permissions.json",
        "sed -i s/a/b/ ~/.claude/settings.json",
        "cp evil.sh .git/hooks/pre-commit",
        "mkdir ~/.claude/skills/new-skill",
    ] {
        assert!(writes_fence_denied_path(cmd), "writes: {cmd}");
    }
}

/// A copy reads its source, so backing a control file up writes only the
/// destination. The source operand sat in a position the old check treated as
/// a write, which refused a backup the kernel would have allowed.
#[test]
fn test_copy_source_is_read() {
    for cmd in [
        "cp ~/.houyicoder/settings.json \"$TMPDIR/settings.backup.json\"",
        "cp -R ~/.houyicoder/settings.json /tmp/backup/",
        "install -m 600 ~/.houyicoder/settings.json /tmp/backup.json",
        "ln -s ~/.houyicoder/settings.json /tmp/link",
        "dd if=~/.houyicoder/settings.json of=/tmp/image.bin",
    ] {
        assert!(!writes_fence_denied_path(cmd), "reads the source: {cmd}");
    }
}

/// A reference file is read to copy times or ownership from, while the names
/// after it are written. Reading it as the mode left the target unchecked, so
/// a command that changes a control file's ownership passed as a read.
#[test]
fn test_reference_target_is_write() {
    for cmd in [
        "chmod --reference=/tmp/ref .claude/settings.json",
        "chown --reference=/tmp/ref ~/.houyicoder/permissions.json",
        "chgrp --reference=/tmp/ref .git/hooks/pre-commit",
    ] {
        assert!(writes_fence_denied_path(cmd), "writes the target: {cmd}");
        assert!(!reads_only_protected(cmd), "not a read: {cmd}");
    }
    for cmd in [
        "chmod --reference=~/.houyicoder/permissions.json /tmp/copy.txt",
        "chown --reference=.claude/settings.json notes.md",
        "chmod --reference ~/.houyicoder/permissions.json /tmp/copy.txt",
    ] {
        assert!(!writes_fence_denied_path(cmd), "reads the reference: {cmd}");
    }
}

/// The in-place option comes in a long spelling and as a cluster of short
/// letters, and the interpreter spellings differ from the editor's. Matching
/// only the bare short form left the other spellings reading as plain reads.
#[test]
fn test_in_place_edit_detected() {
    for cmd in [
        "sed --in-place s/a/b/ ~/.claude/settings.json",
        "sed --in-place=BAK s/a/b/ ~/.houyicoder/permissions.json",
        "perl -pi -e 's/a/b/' .git/hooks/pre-commit",
        "ruby -i -e 'File.write(\"x\")' ~/.claude/settings.json",
        "sed -i.bak s/a/b/ ~/.houyicoder/settings.json",
    ] {
        assert!(writes_fence_denied_path(cmd), "writes in place: {cmd}");
    }
}

/// Program text handed to an option is not a file the command rewrites. The
/// expression option's value sat in an operand position, so an edit whose
/// pattern merely quoted a control path read as a write to that path.
#[test]
fn test_expression_is_not_file() {
    for cmd in [
        "sed -e 's|.houyicoder/settings.json|x|' docs/notes.md",
        "sed --expression 's|.claude/settings.json|x|' docs/notes.md",
        "sed -e s/a/b/ -e 's|.git/hooks|x|' docs/notes.md",
        "perl -i -e 's|.houyicoder/settings.json|x|' docs/notes.md",
    ] {
        assert!(!writes_fence_denied_path(cmd), "edits another file: {cmd}");
    }
}

/// A move takes both operands away and writes both, so its source counts.
#[test]
fn test_move_source_is_write() {
    assert!(writes_fence_denied_path(
        "mv ~/.houyicoder/settings.json /tmp/settings.json"
    ));
}

/// A script that binds the settings path to a name and replaces that name
/// writes the file. The write call names the variable, so the path appears on
/// an earlier line, and the command used to pass as a read.
#[test]
fn test_script_replace_denied() {
    let command = "cd /Users/von/workspace/hicoder && cat > \"$TMPDIR/cleanup.py\" <<'PY'\nimport json, os\n\npath = os.path.expanduser(\"~/.houyicoder/settings.json\")\nwith open(path) as f:\n    d = json.load(f)\n\ntmp = path + \".cleanup.tmp\"\nwith open(tmp, \"w\") as f:\n    json.dump(d, f)\n\nos.replace(tmp, path)\nPY\npython3 \"$TMPDIR/cleanup.py\"";
    assert!(
        writes_fence_denied_path(command),
        "the script replaces the settings file it bound"
    );
}

/// The same script without a write path only reads the settings file, so the
/// protected-path question does not stand in front of it.
#[test]
fn test_script_read_not_denied() {
    let command = "python3 -c \"\nimport json,os\nd=json.load(open(os.path.expanduser('~/.houyicoder/settings.json')))\nprint('entries:', len(d.get('trusted_projects',{})))\n\"";
    assert!(!writes_fence_denied_path(command));
}

/// A script may back the file up before reading it. A copy writes its
/// destination, so a backup of the control file is not a write to it.
#[test]
fn test_script_backup_not_denied() {
    let command = "cat > \"$TMPDIR/backup.sh\" <<'SH'\npath = os.path.expanduser(\"~/.houyicoder/settings.json\")\nbak = path + \".bak\"\nshutil.copy2(path, bak)\nSH\nbash \"$TMPDIR/backup.sh\"";
    assert!(!writes_fence_denied_path(command));
}

/// A control child the fence lets a process write stays the user's answer.
/// Refusing one would promise a kernel denial that never comes, and the write
/// would have gone through had the user said yes.
///
/// The commands are read as text by the classifier alone: nothing here starts a
/// shell, so no real configuration file is touched.
#[test]
fn test_unfenced_control_child_asks() {
    for cmd in [
        "echo x > .claude/hooks/pre-commit",
        "echo x > .claude/permissions.json",
        "echo x > .agents/settings.json",
        "echo x > .claude/snapshots/note",
    ] {
        assert!(!writes_fence_denied_path(cmd), "not fence-denied: {cmd}");
    }
}
