//! CLI argument parsing: the parsed command enum + the pure parse function +
//! the usage text. Split from main.rs so that file stays under the size gate.
//! Parsing is pure (no I/O, no process exit) so the full flag/subcommand
//! matrix is unit-testable from main_tests.

use std::vec::IntoIter;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CliCommand {
    /// Run the interactive TUI (default when no mode flag is given).
    Tui {
        project: Option<String>,
        model: Option<String>,
        no_sandbox: bool,
    },
    /// Run the ACP server over stdio instead of the TUI.
    Acp {
        project: Option<String>,
        model: Option<String>,
        no_sandbox: bool,
    },
    /// Run a detached session bound to a local endpoint.
    Serve {
        project: Option<String>,
        /// Custom endpoint; None means the conventional per-user one.
        endpoint: Option<String>,
        model: Option<String>,
        no_sandbox: bool,
    },
    /// Attach a TUI to a detached session.
    Attach {
        /// The endpoint to connect to. None means the session's recorded
        /// endpoint, which the attach entry resolves before connecting.
        endpoint: Option<String>,
        session: String,
        /// Pipe the Agent Client Protocol over stdio instead of running the
        /// TUI: the process bridges an external ACP client to the host.
        acp: bool,
    },
    /// List detached session ids + pids.
    Ps,
    /// Resume a session. --resume <file> resumes from an exported transcript
    /// (a one-time bootstrap); --resume <sid> resumes a session already on
    /// disk. --fork-session (a modifier) mints a new sid seeded from the
    /// source instead of continuing the source itself, so the original is
    /// untouched. The file branch forks a fresh sid regardless (unique).
    Resume {
        value: String,
        project: Option<String>,
        fork: bool,
        no_sandbox: bool,
    },
    /// Continue the most-recently-active session (--continue): resolve the
    /// latest-mtime session on disk and resume it, no sid needed. --fork-
    /// session mints a new sid seeded from that session instead.
    Continue {
        project: Option<String>,
        fork: bool,
        no_sandbox: bool,
    },
    /// Print usage help.
    Help,
    /// Review or apply the session prune plan. Default (dry-run) prints a
    /// summary; --verbose lists every entry; --apply executes after a typed
    /// confirmation (or --yes non-interactively). A CLI maintenance
    /// subcommand parallel to ps/attach, not a slash command.
    Cleanup {
        apply: bool,
        verbose: bool,
        yes: bool,
    },
    /// Manage the external tool servers: list what the scopes resolve to,
    /// add or remove one entry in the user settings or the workspace's
    /// project file, or probe the enabled ones with a real handshake.
    Mcp { action: McpCommand },
}

/// One mcp subcommand's verb and operands.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum McpCommand {
    /// Print every resolved server: name, scope, transport, enabled state.
    List,
    /// Upsert one server. Exactly one transport spelling is taken: an
    /// endpoint URL, or a command with its arguments after --.
    Add {
        name: String,
        /// user writes the settings file; project writes the workspace file.
        project: bool,
        url: Option<String>,
        command: Vec<String>,
    },
    /// Remove one server from the scope that holds it.
    Remove { name: String, project: bool },
    /// Probe the enabled servers with a real handshake and report each
    /// outcome, its timing, and its diagnostics.
    Doctor,
}

impl CliCommand {
    /// Whether the user launched the session with --no-sandbox to skip the
    /// operating system kernel sandbox fence.
    pub(crate) fn no_sandbox(&self) -> bool {
        match self {
            Self::Tui { no_sandbox, .. }
            | Self::Acp { no_sandbox, .. }
            | Self::Resume { no_sandbox, .. }
            | Self::Continue { no_sandbox, .. } => *no_sandbox,
            Self::Serve { no_sandbox, .. } => *no_sandbox,
            _ => false,
        }
    }
}

/// Parse CLI arguments into a command. Pure function: no I/O, no process
/// exit. Returns Err(message) for usage errors the caller prints to stderr
/// before exiting with code 2.
struct Flags {
    project: Option<String>,
    acp: bool,
    resume: Option<String>,
    continue_flag: bool,
    fork: bool,
    model: Option<String>,
    no_sandbox: bool,
    serve: Option<Option<String>>,
}

fn parse_flag(iter: &mut IntoIter<String>, arg: &str, flags: &mut Flags) -> Result<bool, String> {
    if arg == "--project" || arg == "-p" {
        flags.project = Some(iter.next().ok_or("--project requires a path argument")?);
    } else if arg == "--model" {
        flags.model = Some(iter.next().ok_or("--model requires a model id argument")?);
    } else if arg == "--no-sandbox" {
        flags.no_sandbox = true;
    } else if arg == "--acp" {
        flags.acp = true;
    } else if arg == "--detached" {
        flags.serve = Some(None);
    } else if arg == "--serve" {
        let endpoint = iter
            .next()
            .ok_or("--serve requires an endpoint (or use --detached for the conventional one)")?;
        flags.serve = Some(Some(endpoint));
    } else if arg == "-h" || arg == "--help" {
        return Ok(false);
    } else if arg == "--resume" {
        flags.resume = Some(
            iter.next()
                .ok_or("--resume requires a value (a file path or a session id)")?,
        );
    } else if arg == "--continue" || arg == "-c" {
        flags.continue_flag = true;
    } else if arg == "--fork-session" {
        flags.fork = true;
    } else {
        return Err(format!(
            "unknown argument: {arg} (use --project <path>, --model <id>, --no-sandbox, --acp, --detached, --serve <endpoint>, --resume <file|sid>, --continue, --fork-session, attach, or ps)"
        ));
    }
    Ok(true)
}

fn validate_and_build(flags: Flags) -> Result<CliCommand, String> {
    if flags.resume.is_some() && flags.continue_flag {
        return Err("--resume and --continue are mutually exclusive".into());
    }
    if flags.fork && flags.resume.is_none() && !flags.continue_flag {
        return Err("--fork-session needs --resume <sid> or --continue".into());
    }
    if flags.model.is_some() && (flags.resume.is_some() || flags.continue_flag) {
        return Err(
            "--model is for a fresh session; a resumed session restores its own model".into(),
        );
    }
    if let Some(value) = flags.resume {
        return Ok(CliCommand::Resume {
            value,
            project: flags.project,
            fork: flags.fork,
            no_sandbox: flags.no_sandbox,
        });
    }
    if flags.continue_flag {
        return Ok(CliCommand::Continue {
            project: flags.project,
            fork: flags.fork,
            no_sandbox: flags.no_sandbox,
        });
    }
    if let Some(endpoint) = flags.serve {
        return Ok(CliCommand::Serve {
            project: flags.project,
            endpoint,
            model: flags.model,
            no_sandbox: flags.no_sandbox,
        });
    }
    if flags.acp {
        Ok(CliCommand::Acp {
            project: flags.project,
            model: flags.model,
            no_sandbox: flags.no_sandbox,
        })
    } else {
        Ok(CliCommand::Tui {
            project: flags.project,
            model: flags.model,
            no_sandbox: flags.no_sandbox,
        })
    }
}

/// Parse the mcp subcommand's arguments: the verb plus its operands. Pure,
/// like the top-level parse, so the matrix is unit-testable.
fn parse_mcp_args(args: Vec<String>) -> Result<CliCommand, String> {
    let mut iter = args.into_iter();
    let Some(verb) = iter.next() else {
        return Err(
            "mcp needs a verb: list, add, remove, or doctor (see houyi --help)".to_string(),
        );
    };
    let rest: Vec<String> = iter.collect();
    match verb.as_str() {
        "list" => {
            if !rest.is_empty() {
                return Err("mcp list takes no arguments".to_string());
            }
            Ok(CliCommand::Mcp {
                action: McpCommand::List,
            })
        }
        "doctor" => {
            if !rest.is_empty() {
                return Err("mcp doctor takes no arguments".to_string());
            }
            Ok(CliCommand::Mcp {
                action: McpCommand::Doctor,
            })
        }
        "add" => parse_mcp_add(rest),
        "remove" => parse_mcp_remove(rest),
        other => Err(format!(
            "mcp: unknown verb {other} (use list, add, remove, or doctor)"
        )),
    }
}

/// Parse mcp add: a name, the --project scope flag, and exactly one
/// transport spelling — a --url endpoint or a command with its arguments
/// after --.
fn parse_mcp_add(args: Vec<String>) -> Result<CliCommand, String> {
    let mut name: Option<String> = None;
    let mut project = false;
    let mut url: Option<String> = None;
    let mut command: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--project" {
            project = true;
        } else if arg == "--url" {
            if url.is_some() {
                return Err("mcp add takes one --url, not several".into());
            }
            let endpoint = args.get(i + 1).ok_or("mcp add: --url needs an endpoint")?;
            // A following flag is not an endpoint: swallowing it would
            // quietly turn the scope flag into a nonsense URL.
            if endpoint.starts_with("--") {
                return Err("mcp add: --url needs an endpoint".into());
            }
            url = Some(endpoint.clone());
            i += 1;
        } else if arg == "--" {
            command = args[i + 1..].to_vec();
            break;
        } else if name.is_none() && url.is_none() && command.is_empty() {
            name = Some(arg.to_string());
        } else {
            return Err(format!(
                "mcp add: unexpected argument {arg} (a name, --project, --url <endpoint>, or -- <command> [args...])"
            ));
        }
        i += 1;
    }
    let name = name.ok_or("mcp add needs a server name")?;
    if name.is_empty() {
        return Err("mcp add needs a server name".into());
    }
    if url.is_none() && command.is_empty() {
        return Err(
            "mcp add needs a transport: --url <endpoint> or -- <command> [args...]".to_string(),
        );
    }
    if url.is_some() && !command.is_empty() {
        return Err("mcp add takes one transport: --url or a command, not both".into());
    }
    Ok(CliCommand::Mcp {
        action: McpCommand::Add {
            name,
            project,
            url,
            command,
        },
    })
}

/// Parse mcp remove: a name and the --project scope flag.
fn parse_mcp_remove(args: Vec<String>) -> Result<CliCommand, String> {
    let mut name: Option<String> = None;
    let mut project = false;
    for arg in args {
        if arg == "--project" {
            project = true;
        } else if name.is_none() {
            name = Some(arg);
        } else {
            return Err(format!(
                "mcp remove: unexpected argument {arg} (a name and optionally --project)"
            ));
        }
    }
    let name = name.ok_or("mcp remove needs a server name")?;
    Ok(CliCommand::Mcp {
        action: McpCommand::Remove { name, project },
    })
}

pub(crate) fn parse_args(args: Vec<String>) -> Result<CliCommand, String> {
    let mut iter = args.into_iter();
    let first = iter.next();

    if first.as_deref() == Some("attach") {
        let mut positionals = Vec::new();
        let mut acp = false;
        for arg in iter {
            if arg == "--acp" {
                acp = true;
            } else if arg.starts_with("--") {
                return Err(format!(
                    "attach: unknown argument: {arg} (use --acp to pipe the Agent Client Protocol)"
                ));
            } else {
                positionals.push(arg);
            }
        }
        let (endpoint, session) = match positionals.as_slice() {
            [session] => (None, session.clone()),
            [endpoint, session] => (Some(endpoint.clone()), session.clone()),
            [] => return Err("-- attach needs a session id".to_string()),
            _ => {
                return Err(
                    "attach: too many arguments (attach <session> or attach <endpoint> <session>)"
                        .to_string(),
                );
            }
        };
        return Ok(CliCommand::Attach {
            endpoint,
            session,
            acp,
        });
    }
    if first.as_deref() == Some("ps") {
        return Ok(CliCommand::Ps);
    }
    if first.as_deref() == Some("mcp") {
        return parse_mcp_args(iter.collect());
    }
    if first.as_deref() == Some("cleanup") {
        let mut apply = false;
        let mut verbose = false;
        let mut yes = false;
        for arg in iter.by_ref() {
            if arg == "--apply" {
                apply = true;
            } else if arg == "--verbose" {
                verbose = true;
            } else if arg == "--yes" {
                yes = true;
            } else {
                return Err(format!(
                    "cleanup: unknown argument: {arg} (use --apply to execute, --verbose to list entries, --yes to skip the prompt)"
                ));
            }
        }
        return Ok(CliCommand::Cleanup {
            apply,
            verbose,
            yes,
        });
    }

    let mut rest: Vec<String> = first.into_iter().collect();
    rest.extend(iter);
    let mut iter = rest.into_iter();

    let mut flags = Flags {
        project: None,
        acp: false,
        resume: None,
        continue_flag: false,
        fork: false,
        model: None,
        no_sandbox: false,
        serve: None,
    };

    while let Some(arg) = iter.next() {
        if !parse_flag(&mut iter, &arg, &mut flags)? {
            return Ok(CliCommand::Help);
        }
    }
    validate_and_build(flags)
}

/// Usage help text printed to stderr on -h / --help.
pub(crate) fn print_help() {
    eprintln!(
        "houyi [--project <path>] [--model <id>] [--no-sandbox] [--acp | --detached | --serve <endpoint> | --resume <file|sid> | --continue]\n  --project <path>   sandbox workspace = <path>\n  --model <id>      run with this model id for the session (overrides settings.json; fresh sessions only — a resumed session restores its own model)\n  --no-sandbox      run without operating system sandbox fence\n  --acp             launch the ACP server over stdio instead of the TUI\n  --detached        run detached: bind the conventional per-user endpoint\n                    (prints session_id + pid to stderr); ps lists sessions.\n  --serve <endpoint>  run detached at a custom endpoint (ps uses the\n                    conventional dir, so prefer --detached for discoverability).\n  --resume <file>   resume from an exported transcript file (one-time bootstrap).\n  --resume <sid>   resume a session already on disk by its session id.\n  -c, --continue    resume the most-recently-active session (no sid needed).\n  --fork-session    with --resume/--continue: mint a new sid seeded from the\n                    source so the original is untouched (non-destructive branch).\n  attach <endpoint> <session_id>  connect a TUI to a detached session\n  ps                             list detached session ids + pids (kill <pid> to stop)\n  cleanup [--apply] [--verbose] [--yes]
                   review (or --apply to execute) the prune plan; --verbose lists
                   every entry; --yes skips the prompt (non-interactive)\n  mcp list\n  mcp add <name> [--project] (--url <url> | -- <command> [args...])\n  mcp remove <name> [--project]\n  mcp doctor\n                   manage the external tool servers: list what the scopes\n                   resolve to, add or remove an entry, or probe the enabled\n                   ones with a real handshake"
    );
}
