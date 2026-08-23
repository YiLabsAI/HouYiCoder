//! CLI argument parsing: the parsed command enum + the pure parse function +
//! the usage text. Split from main.rs so that file stays under the size gate.
//! Parsing is pure (no I/O, no process exit) so the full flag/subcommand
//! matrix is unit-testable from main_tests.

#[derive(Debug)]
pub(crate) enum CliCommand {
    /// Run the interactive TUI (default when no mode flag is given).
    Tui {
        project: Option<String>,
        model: Option<String>,
    },
    /// Run the ACP server over stdio instead of the TUI.
    Acp {
        project: Option<String>,
        model: Option<String>,
    },
    /// Run a detached session bound to a Unix domain socket.
    #[cfg(unix)]
    Serve {
        project: Option<String>,
        /// Custom socket path; None means the conventional per-user path.
        socket: Option<String>,
        model: Option<String>,
    },
    /// Attach a TUI to a detached session.
    #[cfg(unix)]
    Attach { socket: String, session: String },
    /// List detached session ids + pids.
    #[cfg(unix)]
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
    },
    /// Continue the most-recently-active session (--continue): resolve the
    /// latest-mtime session on disk and resume it, no sid needed. --fork-
    /// session mints a new sid seeded from that session instead.
    Continue { project: Option<String>, fork: bool },
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
    #[cfg(unix)]
    serve: Option<Option<String>>,
}

fn parse_flag(
    iter: &mut std::vec::IntoIter<String>,
    arg: &str,
    flags: &mut Flags,
) -> Result<bool, String> {
    if arg == "--project" || arg == "-p" {
        flags.project = Some(iter.next().ok_or("--project requires a path argument")?);
    } else if arg == "--model" {
        flags.model = Some(iter.next().ok_or("--model requires a model id argument")?);
    } else if arg == "--acp" {
        flags.acp = true;
    } else if arg == "--detached" {
        #[cfg(unix)]
        {
            flags.serve = Some(None);
        }
        #[cfg(not(unix))]
        {
            return Err(
                "--detached is unix-only; detached sessions need a Unix domain socket".into(),
            );
        }
    } else if arg == "--serve" {
        let path = iter.next().ok_or(
            "--serve requires a socket path (or use --detached for the conventional path)",
        )?;
        #[cfg(unix)]
        {
            flags.serve = Some(Some(path));
        }
        #[cfg(not(unix))]
        {
            drop(path);
            return Err("--serve is unix-only; detached sessions need a Unix domain socket".into());
        }
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
            "unknown argument: {arg} (use --project <path>, --acp, --detached, --serve <socket>, --resume <file|sid>, --continue, --fork-session, attach, or ps)"
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
        });
    }
    if flags.continue_flag {
        return Ok(CliCommand::Continue {
            project: flags.project,
            fork: flags.fork,
        });
    }
    #[cfg(unix)]
    if let Some(socket_opt) = flags.serve {
        return Ok(CliCommand::Serve {
            project: flags.project,
            socket: socket_opt,
            model: flags.model,
        });
    }
    if flags.acp {
        Ok(CliCommand::Acp {
            project: flags.project,
            model: flags.model,
        })
    } else {
        Ok(CliCommand::Tui {
            project: flags.project,
            model: flags.model,
        })
    }
}

pub(crate) fn parse_args(args: Vec<String>) -> Result<CliCommand, String> {
    let mut iter = args.into_iter();
    let first = iter.next();

    #[cfg(unix)]
    if first.as_deref() == Some("attach") {
        let socket = iter.next().ok_or("-- attach needs a socket path")?;
        let session = iter.next().ok_or("-- attach needs a session id")?;
        return Ok(CliCommand::Attach { socket, session });
    }
    #[cfg(unix)]
    if first.as_deref() == Some("ps") {
        return Ok(CliCommand::Ps);
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
        #[cfg(unix)]
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
        "houyi [--project <path>] [--model <id>] [--acp | --detached | --serve <socket> | --resume <file|sid> | --continue]\n  --project <path>   sandbox workspace = <path>\n  --model <id>      run with this model id for the session (overrides settings.json; fresh sessions only — a resumed session restores its own model)\n  --acp             launch the ACP server over stdio instead of the TUI\n  --detached        run detached: bind a conventional per-user socket (prints\n                    session_id + pid to stderr); ps lists detached sessions.\n  --serve <socket>  run detached at a custom socket path (ps uses the\n                    conventional dir, so prefer --detached for discoverability).\n  --resume <file>   resume from an exported transcript file (one-time bootstrap).\n  --resume <sid>   resume a session already on disk by its session id.\n  -c, --continue    resume the most-recently-active session (no sid needed).\n  --fork-session    with --resume/--continue: mint a new sid seeded from the\n                    source so the original is untouched (non-destructive branch).\n  attach <socket> <session_id>  connect a TUI to a detached session\n  ps                             list detached session ids + pids (kill <pid> to stop)\n  cleanup [--apply] [--verbose] [--yes]
                   review (or --apply to execute) the prune plan; --verbose lists
                   every entry; --yes skips the prompt (non-interactive)"
    );
}
