//! The programs that run the command written after their own options, and how
//! their options are stepped over. A launcher missing from the table hides its
//! program from every check, so an unfamiliar name is a coverage gap rather
//! than a neutral default.

use super::{ShellCommand, decode_word, names_program, program_name, split_words_quote_aware};

/// A program that runs the command written after its own options rather than
/// being the command. The launcher and its options are stepped over so the
/// launched program stands where the shell will start it.
struct Launcher {
    name: &'static str,
    /// The launcher's options that consume the following word as their value.
    value_flags: &'static [&'static str],
    /// The bare operands between the options and the program.
    operands_before_program: usize,
    /// Whether the launched program runs with elevated privileges.
    escalates: bool,
}

impl Launcher {
    const fn new(name: &'static str) -> Self {
        Self {
            name,
            value_flags: &[],
            operands_before_program: 0,
            escalates: false,
        }
    }

    const fn with_values(name: &'static str, value_flags: &'static [&'static str]) -> Self {
        Self {
            name,
            value_flags,
            operands_before_program: 0,
            escalates: false,
        }
    }

    const fn with_operand(name: &'static str, value_flags: &'static [&'static str]) -> Self {
        Self {
            name,
            value_flags,
            operands_before_program: 1,
            escalates: false,
        }
    }

    const fn escalating(name: &'static str, value_flags: &'static [&'static str]) -> Self {
        Self {
            name,
            value_flags,
            operands_before_program: 0,
            escalates: true,
        }
    }
}

/// The launchers this scan steps over. The list is read as one rule: a program
/// that runs the words after its own options is stepped over, because the
/// command the shell starts is the one those words name.
const LAUNCHERS: &[Launcher] = &[
    Launcher::new("arch"),
    Launcher::new("busybox"),
    Launcher::with_values("caffeinate", CAFFEINATE_VALUE_FLAGS),
    Launcher::with_operand("chroot", &["--userspec", "--groups"]),
    Launcher::new("command"),
    Launcher::escalating("doas", &["-u"]),
    Launcher::with_values("env", ENV_VALUE_FLAGS),
    Launcher::with_values("exec", &["-a"]),
    Launcher::with_values(
        "fakeroot",
        &[
            "-l",
            "-i",
            "-s",
            "-b",
            "--lib",
            "--faked",
            "--save-file",
            "--fd-base",
        ],
    ),
    Launcher::with_values(
        "ionice",
        &["-c", "-n", "-p", "--class", "--classdata", "--pid"],
    ),
    Launcher::with_values("nice", NICE_VALUE_FLAGS),
    Launcher::new("nohup"),
    Launcher::escalating("pkexec", &["--user"]),
    Launcher::with_values("proxychains", &["-f"]),
    Launcher::with_values("proxychains4", &["-f"]),
    Launcher::with_operand(
        "script",
        &[
            "-c",
            "-e",
            "-t",
            "-o",
            "-I",
            "-O",
            "-T",
            "--command",
            "--log-out",
            "--log-in",
            "--timing",
        ],
    ),
    Launcher::new("setsid"),
    Launcher::with_values(
        "stdbuf",
        &["-i", "-o", "-e", "--input", "--output", "--error"],
    ),
    Launcher::with_values(
        "strace",
        &[
            "-o",
            "-e",
            "-p",
            "-P",
            "-s",
            "-u",
            "-a",
            "-b",
            "-E",
            "-I",
            "-X",
            "-O",
            "--output",
            "--expr",
            "--trace",
            "--attach",
            "--string-limit",
            "--columns",
            "--user",
            "--syscall",
            "--env",
        ],
    ),
    Launcher::escalating(
        "sudo",
        &[
            "-u",
            "-g",
            "-p",
            "-C",
            "-D",
            "-h",
            "-r",
            "-t",
            "-U",
            "-T",
            "--user",
            "--group",
            "--prompt",
            "--close-from",
            "--chdir",
            "--role",
            "--type",
            "--other-user",
            "--host",
        ],
    ),
    Launcher::with_operand("taskset", &["-c", "--cpu-list"]),
    Launcher::with_values("time", TIME_VALUE_FLAGS),
    Launcher::with_values("timeout", TIMEOUT_VALUE_FLAGS),
    Launcher::new("unbuffer"),
    Launcher::with_values("watch", &["-n", "--interval"]),
    Launcher::with_values(
        "xvfb-run",
        &[
            "-e",
            "-f",
            "-n",
            "-p",
            "-s",
            "-w",
            "--error-file",
            "--auth-file",
            "--server-num",
            "--server-protocol",
            "--server-args",
            "--wait",
        ],
    ),
];

/// The wrapper options that consume the following word as their value.
const TIMEOUT_VALUE_FLAGS: &[&str] = &["-k", "-s", "--kill-after", "--signal"];
const ENV_VALUE_FLAGS: &[&str] = &["-C", "-u", "--chdir", "--unset"];
const NICE_VALUE_FLAGS: &[&str] = &["-n", "--adjustment"];
const CAFFEINATE_VALUE_FLAGS: &[&str] = &["-t", "-w", "--wait"];
const TIME_VALUE_FLAGS: &[&str] = &["-f", "-o", "--format", "--output"];

/// The launcher a word names, or None when the word is not one this scan can
/// step over.
fn launcher_at(word: &str) -> Option<&'static Launcher> {
    LAUNCHERS
        .iter()
        .find(|launcher| launcher.name.eq_ignore_ascii_case(word))
}

/// Whether the word names a program that runs the command written after its
/// own arguments rather than being the command. A reader walking raw words
/// asks this instead of keeping a second list that drifts from this one.
pub(crate) fn is_launcher(word: &str) -> bool {
    launcher_at(word).is_some()
}

/// Whether the launcher at this word runs its program with elevated
/// privileges, so the command that follows carries that escalation.
pub(super) fn launcher_escalates(word: &str) -> bool {
    launcher_at(word).is_some_and(|launcher| launcher.escalates)
}

/// The index of the program a launcher at index runs, or None when the word is
/// not a launcher this scan can step over.
pub(super) fn wrapped_index(words: &[String], index: usize) -> Option<usize> {
    let launcher = launcher_at(&words[index])?;
    // A query about a program does not run it, so the words after it are not a
    // command the shell starts.
    if launcher.name == "command"
        && words[index + 1..]
            .iter()
            .any(|word| matches!(word.as_str(), "-v" | "-V"))
    {
        return None;
    }
    let mut next = skip_options(words, index + 1, launcher.value_flags);
    match launcher.name {
        // The duration follows timeout's options and is not the program.
        "timeout" => next += 1,
        "env" => {
            while next < words.len() && is_assignment_word(&words[next]) {
                next += 1;
            }
        }
        // A CPU list given as an option already consumed the mask, so no bare
        // operand stands between the options and the program.
        "taskset"
            if words[index + 1..next].iter().any(|word| {
                word == "-c" || word == "--cpu-list" || word.starts_with("--cpu-list=")
            }) => {}
        _ => next += launcher.operands_before_program,
    }
    (next > index && next < words.len()).then_some(next)
}

/// The command env's split-string option carries as one value.
///
/// The option is not a value env passes through: both env and the shell split
/// the text into words before running it, so those words are the command. The
/// returned count is how many words the launcher and its option occupy,
/// measured from the launcher word, and the vector is the command that
/// replaces them.
pub(super) fn split_string_value(words: &[String], index: usize) -> Option<(usize, Vec<String>)> {
    if !words[index].eq_ignore_ascii_case("env") {
        return None;
    }
    let mut at = index + 1;
    while at < words.len() {
        let word = words[at].as_str();
        let (consumed, value) = if word == "-S" || word == "--split-string" {
            (at + 2 - index, words.get(at + 1)?.clone())
        } else if let Some(rest) = word.strip_prefix("--split-string=") {
            (at + 1 - index, rest.to_string())
        } else if word.starts_with('-') {
            at += if ENV_VALUE_FLAGS.contains(&word) {
                2
            } else {
                1
            };
            continue;
        } else {
            return None;
        };
        let split = split_words_quote_aware(&value)
            .into_iter()
            .map(|word| decode_word(&word))
            .collect();
        return Some((consumed, split));
    }
    None
}

/// Step past a run of options. A known option that takes a separate value
/// consumes the next word; an option carrying its own value in the same word
/// consumes one; the first word that is not an option ends the run.
pub(super) fn skip_options(words: &[String], start: usize, value_flags: &[&str]) -> usize {
    let mut index = start;
    while index < words.len() {
        let word = &words[index];
        if word == "-" || !word.starts_with('-') {
            break;
        }
        if word.contains('=') {
            index += 1;
            continue;
        }
        if value_flags.contains(&word.as_str()) {
            index += 2;
        } else {
            index += 1;
        }
    }
    index
}

/// Whether a word is a leading environment assignment rather than a command.
pub(super) fn is_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The runtimes whose exec subcommand runs a command in a container.
const CONTAINER_RUNNERS: &[&str] = &["docker", "kubectl", "nerdctl", "podman"];

/// The exec options that consume the following word as their value.
const CONTAINER_EXEC_VALUE_FLAGS: &[&str] = &[
    "-c",
    "-e",
    "-n",
    "-u",
    "-w",
    "--attach",
    "--container",
    "--context",
    "--detach-keys",
    "--env",
    "--env-file",
    "--filename",
    "--kubeconfig",
    "--namespace",
    "--pod-running-timeout",
    "--request-timeout",
    "--user",
    "--workdir",
];

/// The index of the command a container runtime runs, when the invocation is
/// its exec subcommand: the runtime, the subcommand, its options, the
/// container or pod operand, and an optional separator.
pub(super) fn container_command_index(words: &[String], index: usize) -> Option<usize> {
    if !CONTAINER_RUNNERS
        .iter()
        .any(|runner| names_program(&words[index], runner))
    {
        return None;
    }
    if !words.get(index + 1)?.eq_ignore_ascii_case("exec") {
        return None;
    }
    let mut at = skip_options(words, index + 2, CONTAINER_EXEC_VALUE_FLAGS);
    // The container or pod the command runs in.
    at += 1;
    if words.get(at).is_some_and(|word| word == "--") {
        at += 1;
    }
    (at < words.len()).then_some(at)
}

/// The runner programs whose arguments are another command, and the
/// subcommand each runs it through when it needs one.
const RUNNERS: &[(&str, Option<&str>)] = &[
    ("bunx", None),
    ("deno", Some("run")),
    ("npx", None),
    ("pipx", Some("run")),
    ("pnpm", Some("dlx")),
    ("pnpm", Some("exec")),
    ("poetry", Some("run")),
    ("rye", Some("run")),
    ("uv", Some("run")),
    ("yarn", Some("dlx")),
    ("yarn", Some("exec")),
];

/// The parallel runner's options that consume the following word as their
/// value.
const PARALLEL_VALUE_FLAGS: &[&str] = &[
    "-j",
    "-n",
    "-N",
    "-a",
    "-S",
    "-P",
    "-C",
    "-I",
    "-L",
    "--arg-file",
    "--colsep",
    "--delay",
    "--env",
    "--joblog",
    "--jobs",
    "--max-args",
    "--max-lines",
    "--max-procs",
    "--replace",
    "--results",
    "--sshlogin",
    "--sshloginfile",
    "--timeout",
    "--tmpdir",
    "--workdir",
];

/// The xargs options that consume the following word as their value.
const XARGS_VALUE_FLAGS: &[&str] = &[
    "-E",
    "-I",
    "-J",
    "-L",
    "-P",
    "-a",
    "-d",
    "-n",
    "-s",
    "--arg-file",
    "--delimiter",
    "--max-args",
    "--max-procs",
    "--replace",
];

/// The command text a runner hands its arguments to, when the stage runs one.
/// The words are joined so the caller reads them as shell text; a runner that
/// needs a subcommand contributes only the words after it, and a runner given
/// nothing runs nothing.
pub(super) fn runner_command_text(command: &ShellCommand) -> Option<String> {
    let program = program_name(command.program()).to_ascii_lowercase();
    let words = command.args();
    let rest = if program == "parallel" {
        // The command sits between the runner's options and the argument
        // separator; words after the separator are data.
        let start = skip_options(words, 0, PARALLEL_VALUE_FLAGS);
        let end = words[start..]
            .iter()
            .position(|word| word == ":::" || word.starts_with(":::"))
            .map(|offset| start + offset)
            .unwrap_or(words.len());
        &words[start..end]
    } else {
        let (_, subcommand) = RUNNERS.iter().find(|(name, _)| *name == program)?;
        match subcommand {
            Some(sub) if words.first().is_some_and(|word| word == sub) => &words[1..],
            Some(_) => return None,
            None => words,
        }
    };
    (!rest.is_empty()).then(|| rest.join(" "))
}

/// The programs a stage runs in argument position: xargs runs the words after
/// its options, and find runs each of its exec actions.
pub(super) fn argument_programs(command: &ShellCommand) -> Vec<ShellCommand> {
    let mut out = Vec::new();
    let words = command.args();
    if command.runs("xargs") {
        let start = skip_options(words, 0, XARGS_VALUE_FLAGS);
        if start < words.len() {
            out.push(ShellCommand {
                words: words[start..].to_vec(),
                escalated: command.escalated.clone(),
            });
        }
    }
    if command.runs("find") {
        let mut index = 0;
        while index < words.len() {
            if matches!(
                words[index].as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir"
            ) {
                let start = index + 1;
                let mut end = start;
                while end < words.len() && !matches!(words[end].as_str(), "+" | ";" | "\\;") {
                    end += 1;
                }
                if start < end {
                    out.push(ShellCommand {
                        words: words[start..end].to_vec(),
                        escalated: command.escalated.clone(),
                    });
                }
                index = end + 1;
                continue;
            }
            index += 1;
        }
    }
    out
}
