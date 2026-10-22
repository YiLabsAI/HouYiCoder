use super::*;
use std::env::temp_dir;
use std::fs::write;
use std::process::id;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// A ShellSnapshot whose snapshot path is an existing empty temp file, so
/// ensure() returns Some without spawning a real login shell. The inject
/// unit tests verify the wrap string (source + export + eval), not
/// snapshot creation; a stub path that does not exist made ensure() fall
/// to create(), which spawns a login shell - flaky in environments where
/// that spawn fails (check-full). Returns the stub path so the caller
/// cleans it up.
fn snap_with_stub() -> (ShellSnapshot, PathBuf) {
    let mut snap = ShellSnapshot::new(PathBuf::from("/bin/zsh"), PathBuf::from("/tmp"));
    let path = temp_dir().join(format!(
        "houyi-snap-stub-{}-{}",
        id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    write(&path, "").expect("write stub snapshot");
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
    let mut snap = ShellSnapshot::new(PathBuf::from("/nonexistent/shell"), PathBuf::from("/tmp"));
    let out = snap.inject("echo hi", None);
    assert!(out.contains("echo hi"));
}

#[test]
fn test_dump_script_exports_path() {
    let snap = ShellSnapshot::new(PathBuf::from("/bin/zsh"), PathBuf::from("/tmp"));
    let script = snap.dump_script(Path::new("/tmp/snap.sh"), Path::new("/tmp/.zshrc"), false);
    assert!(
        script.contains("grep -v '^export -'"),
        "zsh flag exports must be stripped before bash sources the file: {script}"
    );
    assert!(
        script.contains("export PATH=%q"),
        "PATH must be emitted as a scalar export for bash: {script}"
    );
}
