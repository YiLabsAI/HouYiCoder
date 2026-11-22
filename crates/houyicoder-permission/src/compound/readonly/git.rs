//! Read-only classification for git repository and configuration subcommands.

use super::has_short_flag;

/// Repository subcommands that only inspect a repository. The hash form is
/// included because without its write flag it prints the hash instead of
/// storing the object; the writing form is rejected by the argument rule.
const GIT_READ_SUBCOMMANDS: &[&str] = &[
    "status",
    "diff",
    "show",
    "log",
    "ls-files",
    "ls-tree",
    "rev-parse",
    "rev-list",
    "hash-object",
    "check-ignore",
    "check-attr",
    "describe",
    "merge-base",
    "for-each-ref",
    // A fetch or a remote listing against a configured remote only moves local
    // refs. A call that names an outside source is caught by the network check,
    // which runs before this one, so the destination is never assumed here.
    "fetch",
    "ls-remote",
];

/// Repository options that write a file, run a program the user did not
/// choose, or inject configuration. Matched by prefix so an abbreviation that
/// could reach one of them is treated as the writer it may be.
const GIT_WRITE_OPTIONS: &[&str] = &[
    "-o",
    "--output",
    "--ext-diff",
    "--textconv",
    "-c",
    "--exec-path",
];

pub(super) fn git_reads_only(args: &[&str]) -> bool {
    if args.iter().any(|arg| git_writes(arg)) {
        return false;
    }
    let mut idx = 0;
    while idx < args.len() {
        let arg = args[idx];
        if matches!(arg, "-C" | "--git-dir" | "--work-tree") {
            if args.get(idx + 1).is_none() {
                return false;
            }
            idx += 2;
            continue;
        }
        if matches!(arg, "--no-pager" | "-P" | "--bare") {
            idx += 1;
            continue;
        }
        break;
    }
    let Some(&sub) = args.get(idx) else {
        return false;
    };
    let rest = &args[idx + 1..];
    match sub {
        "config" => return git_config_reads_only(rest),
        "stash" => {
            let sub_verb = rest.iter().find(|a| !a.starts_with('-')).copied();
            return matches!(sub_verb, Some("list" | "show"));
        }
        "hash-object" => return !rest.iter().any(|arg| has_short_flag(arg, 'w')),
        _ => {}
    }
    GIT_READ_SUBCOMMANDS.contains(&sub)
}

/// Whether arguments passed to git config strictly query existing values
/// without modifying or opening an editor on any configuration file.
pub(crate) fn git_config_reads_only(args: &[&str]) -> bool {
    const VALUE_FLAGS: &[&str] = &["--type", "-t", "--default", "--color"];
    const WRITE_FLAGS: &[&str] = &[
        "--add",
        "--replace-all",
        "--unset",
        "--unset-all",
        "--rename-section",
        "--remove-section",
        "--edit",
        "-e",
        "--file",
        "-f",
        "--blob",
    ];
    const READ_FLAGS: &[&str] = &[
        "--get",
        "--get-all",
        "--get-regexp",
        "--get-urlmatch",
        "--list",
        "-l",
    ];
    let mut saw_read_flag = false;
    let mut saw_read_subcommand = false;
    let mut positional_count = 0usize;
    let mut options_done = false;
    let mut expect_value = false;

    for &arg in args {
        if expect_value {
            expect_value = false;
            continue;
        }
        if !options_done {
            if arg == "--" {
                options_done = true;
                continue;
            }
            if WRITE_FLAGS.contains(&arg)
                || arg.starts_with("--file=")
                || arg.starts_with("--blob=")
                || (!arg.starts_with("--")
                    && arg.starts_with('-')
                    && (arg.contains('e') || arg.contains('f')))
            {
                return false;
            }
            if READ_FLAGS.contains(&arg) {
                saw_read_flag = true;
                continue;
            }
            if VALUE_FLAGS.contains(&arg) {
                expect_value = true;
                continue;
            }
            if arg.starts_with('-') {
                continue;
            }
        }
        if !saw_read_flag && !saw_read_subcommand && positional_count == 0 {
            match arg {
                "list" | "get" => {
                    saw_read_subcommand = true;
                    continue;
                }
                "set" | "unset" | "rename-section" | "remove-section" | "edit" => {
                    return false;
                }
                _ => {}
            }
        }
        positional_count += 1;
    }
    if expect_value {
        return false;
    }
    saw_read_flag || saw_read_subcommand || positional_count == 1
}

/// Whether one repository argument asks for a write, an outside program, or
/// injected configuration.
fn git_writes(arg: &str) -> bool {
    if let Some(long) = arg.strip_prefix("--") {
        return GIT_WRITE_OPTIONS.iter().any(|option| {
            option
                .strip_prefix("--")
                .is_some_and(|name| long == name || long.starts_with(&format!("{name}=")))
        });
    }
    let Some(short) = arg.strip_prefix('-') else {
        return false;
    };
    GIT_WRITE_OPTIONS
        .iter()
        .filter_map(|option| option.strip_prefix('-'))
        .any(|letter| short.starts_with(letter))
}
