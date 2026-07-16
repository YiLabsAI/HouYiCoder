//! Login-shell environment snapshot: capture PATH, aliases, functions, and
//! options once per session by running a login shell that sources the user's
//! rc file, dump the resulting environment to a file under the config home,
//! and source that file into subsequent sandboxed commands so they run with
//! the user's expected environment without paying a login-shell init on every
//! spawn.
//!
//! Why: the sandboxed shell runs non-login (sh -c) to avoid the
//! etc/profile Operation-not-permitted noise a login shell would trip under
//! the fence. The cost is that PATH (homebrew opt/homebrew/bin, the etc/paths.d
//! entries, user aliases and functions) is lost, and commands the agent needs
//! are not found. This snapshot is the fix: one login run captures the env,
//! later commands source the snapshot. The snapshot file lives under the
//! config home (not tmpdir) so it survives a system tmp clean, and a missing
//! file is rebuilt on the next command rather than falling back to a login
//! shell every time.
//!
//! The snapshot creation runs HOST-SIDE (unsandboxed): it must read
//! etc/profile and the rc file freely, which the fence would deny. The
//! snapshot is then sourced INSIDE the sandboxed command, where the fence
//! allow-backs the config-home path.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A captured login-shell environment. Owns the snapshot file path; inject
/// wraps a command so the sandboxed shell sources it before running.
#[derive(Debug)]
pub struct ShellSnapshot {
    /// None until the first command triggers creation; None again if the
    /// file vanishes (rebuilt on the next ensure).
    path: Option<PathBuf>,
    shell: PathBuf,
    home: PathBuf,
}

impl ShellSnapshot {
    pub fn new(shell: PathBuf, home: PathBuf) -> Self {
        Self {
            path: None,
            shell,
            home,
        }
    }

    /// The shell config file to source (zshrc for zsh, bashrc for bash,
    /// profile otherwise).
    fn config_file(&self) -> PathBuf {
        let name = if self.shell.to_string_lossy().contains("zsh") {
            ".zshrc"
        } else if self.shell.to_string_lossy().contains("bash") {
            ".bashrc"
        } else {
            ".profile"
        };
        self.home.join(name)
    }

    /// Return the snapshot path, creating it if missing. Returns None if
    /// creation fails (the command then runs unsnapshotted, degraded, not
    /// fatal).
    pub fn ensure(&mut self) -> Option<PathBuf> {
        if let Some(p) = &self.path
            && p.exists()
        {
            return Some(p.clone());
        }
        // Rebuild: the snapshot file is gone (tmp clean, manual rm, first
        // run). Create a fresh one. Creation here is synchronous and
        // repeatable, so a lost snapshot is rebuilt on the next command
        // rather than falling back to a login shell on every command.
        match self.create() {
            Ok(p) => {
                self.path = Some(p.clone());
                Some(p)
            }
            Err(_) => {
                self.path = None;
                None
            }
        }
    }

    /// Run a login shell once, sourcing the user's rc file, and dump the
    /// resulting env (functions, options, aliases, PATH) to a snapshot file.
    #[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
    fn create(&self) -> std::io::Result<PathBuf> {
        let dir = self.home.join(".houyicoder").join("shell-snapshots");
        std::fs::create_dir_all(&dir)?;
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let shell_tag = self
            .shell
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("sh");
        let path = dir.join(format!("snapshot-{shell_tag}-{pid}-{nanos}.sh"));
        let config = self.config_file();
        let config_exists = config.exists();
        let script = self.dump_script(&path, &config, config_exists);
        // HOST-SIDE, unsandboxed: the login shell must read etc/profile and
        // the rc file freely. A slow rc (huge zshrc, network call in startup)
        // could block; the spawn is best-effort and a hung rc is rare enough
        // to accept for the one-time capture rather than pull a wait-timeout
        // dep. TODO: bound with a child-wait timeout if a real rc hang shows.
        let out = Command::new(&self.shell)
            .arg("-c")
            .arg("-l")
            .arg(&script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                return Err(std::io::Error::other(format!("snapshot spawn failed: {e}")));
            }
        };
        if !out.status.success() || !path.exists() {
            return Err(std::io::Error::other(format!(
                "snapshot creation failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(path)
    }

    /// The script a login shell runs: source the rc file, then append the
    /// dumped environment to the snapshot file (zsh typeset -f, bash declare -f
    /// base64).
    fn dump_script(&self, snapshot: &Path, config: &Path, config_exists: bool) -> String {
        let snap = snapshot.to_string_lossy();
        let config_line = if config_exists {
            format!("source {:?} < /dev/null", config.to_string_lossy())
        } else {
            "# No user config file to source".to_string()
        };
        // Dump only exported environment variables (export -p produces
        // POSIX-compatible export VAR=value lines, including PATH). Functions,
        // shell options, and aliases are shell-specific (zsh typeset output
        // will not parse in the bash sh that runs the sandboxed command), so
        // they are skipped to avoid a parse error that aborts the source.
        // PATH is the user-reported loss; the exported env covers it. Functions
        // and aliases are a future item once the sandboxed shell matches the
        // snapshot-creation shell.
        let dump = r##"
          export -p >> "$SNAPSHOT_FILE"
        "##;
        format!(
            "SNAPSHOT_FILE={snap:?}\n{config_line}\n\
             echo \"# Snapshot file\" >| \"$SNAPSHOT_FILE\"\n\
             {dump}\n\
             if [ ! -f \"$SNAPSHOT_FILE\" ]; then echo \"Error: snapshot not created\" >&2; exit 1; fi"
        )
    }

    /// Wrap a command so the sandboxed shell sources the snapshot first. The
    /// stderr-redirect plus or-true guards the race between the ensure check
    /// and the spawn. eval gives a second parse pass so sourced aliases
    /// expand. If no snapshot is available, the command runs unchanged
    /// (degraded).
    ///
    /// The command is single-quote-wrapped (escaping embedded single quotes
    /// via the close-quote-escaped-literal-quote-reopen idiom) before eval,
    /// not Rust Debug-quoted. Debug formatting escapes a real newline to the
    /// two literal characters backslash-n, so eval of a heredoc or multiline
    /// minus-c string collapsed the body onto one line and broke it. Single
    /// quotes preserve newlines verbatim; eval re-parses the unquoted content
    /// so sourced aliases still expand and for-loops and heredocs parse
    /// normally.
    ///
    /// When a per-session tmpdir is supplied, TMPDIR and TMPPREFIX are
    /// re-exported AFTER sourcing the snapshot. The snapshot is built from a
    /// login shell export -p, which captures the host TMPDIR; sourcing it
    /// would clobber the spawn-time env set on the child, leaving TMPDIR
    /// pointing at the host temp root (outside the fence). Re-asserting the
    /// per-session values keeps heredoc temp files and tools that honor TMPDIR
    /// inside the fence. TMPPREFIX routes zsh heredoc temp (zsh ignores TMPDIR
    /// for that) to the same dir.
    pub fn inject(&mut self, command: &str, tmpdir: Option<&str>) -> String {
        match self.ensure() {
            Some(p) => {
                let p = p.to_string_lossy();
                let quoted = shell_quote_single(command);
                let reassert = match tmpdir {
                    Some(t) => format!(" && export TMPDIR='{t}' TMPPREFIX='{t}/zsh'"),
                    None => String::new(),
                };
                format!(
                    "source {p:?} 2>/dev/null || true{reassert} && {{ shopt -u extglob 2>/dev/null || setopt NO_EXTENDED_GLOB 2>/dev/null || true; }} && eval {quoted}"
                )
            }
            None => command.to_string(),
        }
    }
}

/// Wrap a string in single quotes for eval, escaping embedded single quotes
/// via the close-quote-escaped-literal-quote-reopen idiom. Used for heredoc
/// and multiline commands; it preserves real newlines, which Rust Debug
/// formatting does not.
fn shell_quote_single(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

impl Drop for ShellSnapshot {
    fn drop(&mut self) {
        // Best-effort cleanup; never panic on drop.
        if let Some(p) = &self.path {
            drop(std::fs::remove_file(p));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ShellSnapshot whose snapshot path is an existing empty temp file, so
    /// ensure() returns Some without spawning a real login shell. The inject
    /// unit tests verify the wrap string (source + export + eval), not
    /// snapshot creation; a stub path that does not exist made ensure() fall
    /// to create(), which spawns a login shell - flaky in environments where
    /// that spawn fails (check-full). Returns the stub path so the caller
    /// cleans it up.
    fn snap_with_stub() -> (ShellSnapshot, PathBuf) {
        let mut snap = ShellSnapshot::new(PathBuf::from("/bin/zsh"), PathBuf::from("/tmp"));
        let path = std::env::temp_dir().join(format!(
            "houyi-snap-stub-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&path, "").expect("write stub snapshot");
        snap.path = Some(path.clone());
        (snap, path)
    }

    #[test]
    fn test_inject_wraps_source_eval() {
        // With a stub snapshot path, inject wraps the command.
        let (mut snap, _stub) = snap_with_stub();
        let out = snap.inject("ls -la", None);
        assert!(
            out.contains("source "),
            "inject sources the snapshot: {out}"
        );
        assert!(out.contains("eval "), "inject evals the command: {out}");
        assert!(
            out.contains("ls -la"),
            "inject preserves the command: {out}"
        );
    }

    #[test]
    fn test_inject_reasserts_after_source() {
        // The snapshot dump (export -p) captures the host TMPDIR; sourcing it
        // would clobber the spawn-time env. inject re-exports TMPDIR +
        // TMPPREFIX AFTER the source so the per-session values win.
        let (mut snap, _stub) = snap_with_stub();
        let out = snap.inject("ls -la", Some("/tmp/houyi-xyz"));
        assert!(
            out.contains("export TMPDIR='/tmp/houyi-xyz'"),
            "re-asserts TMPDIR after source: {out}"
        );
        assert!(
            out.contains("TMPPREFIX='/tmp/houyi-xyz/zsh'"),
            "re-asserts TMPPREFIX for zsh heredoc temp: {out}"
        );
        let src_pos = out.find("source ").unwrap();
        let exp_pos = out.find("export TMPDIR").unwrap();
        assert!(exp_pos > src_pos, "export after source: {out}");
    }

    #[test]
    fn test_inject_preserves_real_newlines() {
        // Debug formatting would escape \n to literal backslash-n, collapsing
        // heredoc bodies and multiline -c strings onto one line. The
        // single-quote wrap must keep real newlines so eval parses the
        // heredoc body on its own lines.
        let (mut snap, _stub) = snap_with_stub();
        let cmd = "python3 << 'EOF'\nprint(1)\nEOF";
        let out = snap.inject(cmd, None);
        assert!(out.contains('\n'), "inject keeps real newline: {out}");
        assert!(
            !out.contains("\\n"),
            "inject must not escape newline to literal backslash-n: {out}"
        );
        assert!(
            out.contains("print(1)"),
            "heredoc body preserved verbatim: {out}"
        );
    }

    #[test]
    fn test_inject_escapes_single_quotes() {
        // A command containing single quotes must survive eval: the embedded
        // quotes are escaped via the '"'"' idiom so eval reconstitutes them.
        let (mut snap, _stub) = snap_with_stub();
        let out = snap.inject("echo 'it works'", None);
        assert!(out.contains("it works"), "command text preserved: {out}");
        assert!(
            out.contains("'\"'\"'"),
            "embedded single quotes escaped via idiom: {out}"
        );
    }

    #[test]
    fn test_inject_without_passes_through() {
        // ensure() fails (no shell can create a snapshot here) -> None ->
        // degraded pass-through.
        let mut snap =
            ShellSnapshot::new(PathBuf::from("/nonexistent/shell"), PathBuf::from("/tmp"));
        let out = snap.inject("echo hi", None);
        assert!(out.contains("echo hi"));
    }
}
