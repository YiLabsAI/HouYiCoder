//! Per-spawn Landlock fence applicator for the Linux sandbox.
//!
//! The daemon spawns this helper for every sandboxed command and fence
//! probe. It applies a Landlock ruleset to itself, then replaces its own
//! image with the shell running the command: the fence covers the shell and
//! its descendants, the daemon is never restricted, and exec preserves the
//! process group so the parent's group-kill semantics pass through.

/// The parsed command line.
#[cfg(target_os = "linux")]
struct ParsedArgs {
    probe: bool,
    reads: Vec<String>,
    writes: Vec<String>,
    command: Option<String>,
}

#[cfg(target_os = "linux")]
fn main() {
    match parse_args() {
        Err(message) => {
            eprintln!("sandbox-helper: {message}");
            std::process::exit(2);
        }
        Ok(parsed) => run(parsed),
    }
}

/// Parse the protocol: --probe, --read dir and --write dir flags, then --
/// followed by the shell command. Probe mode prints one status word to
/// stdout (enforced, enforced-partial, not-enforced, unavailable,
/// failed:reason) and exits. The grant set rides in argv, visible in the
/// process listing to the same user: directories the user authorized, not
/// secrets.
#[cfg(target_os = "linux")]
fn parse_args() -> Result<ParsedArgs, String> {
    let mut parsed = ParsedArgs {
        probe: false,
        reads: Vec::new(),
        writes: Vec::new(),
        command: None,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--probe" => parsed.probe = true,
            "--read" => {
                let dir = argv.next().ok_or("--read needs a directory")?;
                parsed.reads.push(dir);
            }
            "--write" => {
                let dir = argv.next().ok_or("--write needs a directory")?;
                parsed.writes.push(dir);
            }
            "--" => {
                parsed.command = argv.next();
                break;
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    if !parsed.probe && parsed.command.is_none() {
        return Err("no command after --".into());
    }
    Ok(parsed)
}

/// Apply the fence, then exec the command under it. Per-spawn resource limits
/// are deliberately absent: the rlimit primitives are per real user ID
/// (RLIMIT_NPROC counts every thread of the user, so a fenced shell on a
/// normal desktop fails its first fork) or per virtual address space
/// (RLIMIT_AS, which compilers and managed runtimes routinely exceed with
/// modest resident memory), not per tree. The daemon's wall timeout plus
/// process-group kill is the resource fence until a per-tree budget exists.
#[cfg(target_os = "linux")]
fn run(parsed: ParsedArgs) {
    // Test-suite escape hatch, debug builds only so a release helper can
    // never be unfenced through the environment. Audit lines go to stderr,
    // which the daemon captures into the command's stderr: never silent.
    #[cfg(debug_assertions)]
    let skipped = std::env::var("HOUYICODER_SANDBOX_NO_ENFORCE").is_ok_and(|v| v == "1");
    #[cfg(not(debug_assertions))]
    let skipped = false;
    let outcome = if skipped {
        fence::Outcome::Unavailable
    } else {
        fence::apply(&parsed.reads, &parsed.writes)
    };
    if parsed.probe {
        match outcome {
            fence::Outcome::Enforced => println!("enforced"),
            fence::Outcome::EnforcedPartial => println!("enforced-partial"),
            fence::Outcome::NotEnforced => println!("not-enforced"),
            fence::Outcome::Unavailable => println!("unavailable"),
            fence::Outcome::Failed(reason) => println!("failed:{reason}"),
        }
        return;
    }
    match outcome {
        fence::Outcome::Enforced => {}
        fence::Outcome::EnforcedPartial => eprintln!(
            "sandbox-helper audit: landlock enforced with a degraded kernel ABI; some filesystem rights are not restricted"
        ),
        fence::Outcome::NotEnforced => eprintln!(
            "sandbox-helper audit: landlock supported but ruleset not enforced; running unfenced"
        ),
        fence::Outcome::Unavailable => {
            if skipped {
                eprintln!(
                    "sandbox-helper audit: landlock enforcement skipped via HOUYICODER_SANDBOX_NO_ENFORCE (debug builds only); running unfenced"
                );
            }
        }
        fence::Outcome::Failed(reason) => {
            // Fail closed: the session reported this fence as Enforced at
            // construction, so a spawn that cannot apply it must not run the
            // command unfenced. The audit line reaches the captured stderr.
            eprintln!(
                "sandbox-helper audit: landlock apply failed: {reason}; refusing to run unfenced"
            );
            std::process::exit(1);
        }
    }
    let command = parsed.command.expect("command checked in parse_args");
    exec_shell(&command);
}

#[cfg(target_os = "linux")]
fn exec_shell(command: &str) -> ! {
    use std::os::unix::process::CommandExt;
    // exec replaces this process image: nothing is spawned, so the process
    // spawn chokepoint does not apply — there is no child to fence or audit.
    #[expect(
        clippy::disallowed_methods,
        reason = "exec replaces this image; no child is spawned"
    )]
    let error = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .exec();
    eprintln!("sandbox-helper: exec failed: {error}");
    std::process::exit(127);
}

#[cfg(all(target_os = "linux", feature = "enforce"))]
mod fence {
    /// What the ruleset application reported back.
    pub enum Outcome {
        Enforced,
        /// The ruleset is live but the running kernel's ABI predates some of
        /// the requested rights, so those rights are not restricted. The core
        /// path grants have existed since the first ABI, so the fence is real.
        EnforcedPartial,
        NotEnforced,
        Unavailable,
        Failed(String),
    }

    /// Apply the Landlock ruleset to this process. A kernel without Landlock
    /// reports NotEnforced and a refused ruleset reports Failed; neither
    /// aborts the command, and the caller's audit line keeps the gap visible.
    pub fn apply(reads: &[String], writes: &[String]) -> Outcome {
        use landlock::{
            ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
            path_beneath_rules,
        };

        // System read-only tree: the shell, the dynamic linker, shared libs,
        // config, process and sysfs views. from_read includes Execute so
        // binaries under these trees can run; the device files the runtime
        // opens are added separately.
        const READ_PATHS: &[&str] = &[
            "/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc", "/proc", "/sys",
        ];
        const DEV_PATHS: &[&str] = &["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"];

        // Target a recent ABI; the crate's default BestEffort compatibility
        // degrades on older kernels instead of failing the ruleset.
        let abi = ABI::V6;
        let result = (|| {
            let ruleset = Ruleset::default().handle_access(AccessFs::from_all(abi))?;
            let created = ruleset.create()?;
            let created = created
                .add_rules(path_beneath_rules(READ_PATHS, AccessFs::from_read(abi)))?
                .add_rules(path_beneath_rules(DEV_PATHS, AccessFs::from_all(abi)))?;
            let created = if writes.is_empty() {
                created
            } else {
                created.add_rules(path_beneath_rules(
                    writes.iter().map(String::as_str),
                    AccessFs::from_all(abi),
                ))?
            };
            let created = if reads.is_empty() {
                created
            } else {
                created.add_rules(path_beneath_rules(
                    reads.iter().map(String::as_str),
                    AccessFs::from_read(abi),
                ))?
            };
            let status = created.restrict_self()?;
            Ok::<_, landlock::RulesetError>(status)
        })();
        match result {
            Ok(status) if matches!(status.ruleset, RulesetStatus::FullyEnforced) => {
                Outcome::Enforced
            }
            Ok(status) if matches!(status.ruleset, RulesetStatus::PartiallyEnforced) => {
                Outcome::EnforcedPartial
            }
            Ok(_) => Outcome::NotEnforced,
            Err(e) => Outcome::Failed(e.to_string()),
        }
    }
}

#[cfg(all(target_os = "linux", not(feature = "enforce")))]
mod fence {
    /// What the ruleset application reported back.
    pub enum Outcome {
        Enforced,
        EnforcedPartial,
        NotEnforced,
        Unavailable,
        Failed(String),
    }

    pub fn apply(_reads: &[String], _writes: &[String]) -> Outcome {
        Outcome::Unavailable
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sandbox-helper: the landlock fence helper is linux-only");
    std::process::exit(2);
}
