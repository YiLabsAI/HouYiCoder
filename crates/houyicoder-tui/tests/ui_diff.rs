//! Real-binary PTY test for structured-diff rendering end-to-end: a scripted
//! edit ToolCall runs the real edit tool against a fixture file under target/
//! (gitignored, and inside the workspace root so confine_path permits the
//! write). Auto mode auto-allows Filesystem side effects, so the edit runs with
//! no approval card; the tool returns a result carrying a top-level diff
//! field, the transcript marks it is_diff, and markers::result_body_rows
//! renders the structured diff (the "Added N lines" summary + line-numbered
//! add/remove rows). This is the (d) PTY interaction leg: the unit count==
//! render tests pin the wrap math at a fixed width but cannot exercise the
//! real render path through the binary (the stashed-width vs actual-width
//! drift class only surfaces at interaction time). This drives that path.
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_diff -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working_in_repo};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Seed a throwaway git repo the binary can run in (a workspace manifest so
/// resolve_project_workspace pins the dir). PTY tests that submit a run
/// should run in an isolated repo, not the developer workspace root -- the
/// root's project state can delay stub delivery past the render timeout.
#[allow(clippy::disallowed_methods)]
fn make_temp_repo(slug: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("houyi-diff-repo-{}-{slug}", std::process::id()));
    drop(std::fs::remove_dir_all(&dir));
    std::fs::create_dir_all(&dir).expect("mkdir repo");
    std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = []\n").expect("write manifest");
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "t@x"][..],
        &["config", "user.name", "t"][..],
        &["add", "Cargo.toml"][..],
        &["commit", "-m", "init", "-q"][..],
    ] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {:?}", args);
    }
    dir
}

/// A fixture file the edit tool can modify. Lives under the temp repo's
/// target/ so confine_path (which permits writes inside the binary's cwd)
/// allows the edit. Unique per run (pid + nanos) so parallel test binaries
/// don't collide; removed on drop.
struct Fixture {
    abs: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new(repo: &Path) -> Self {
        let dir = repo.join("target");
        std::fs::create_dir_all(&dir).expect("mkdir target");
        let name = format!(
            "houyi-pty-diff-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let abs = dir.join(&name);
        std::fs::write(&abs, "fn foo() {\n    let a = 1;\n}\n").expect("write fixture");
        Self {
            abs,
            repo: repo.to_path_buf(),
        }
    }

    /// The path as the edit tool sees it: relative to the binary's cwd (the
    /// temp repo), so confine_path resolves it inside the repo.
    fn rel(&self) -> String {
        self.abs
            .strip_prefix(&self.repo)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| String::from("target/unknown"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_file(&self.abs);
    }
}

/// Script template: a scripted edit against the fixture, then plain text so
/// the run ends cleanly (a stateless stub would re-emit the ToolCall every
/// call and loop). The __PATH__ placeholder is the fixture's workspace-relative
/// path. Built with replace (not format!) so the JSON braces need no escaping.
const EDIT_SCRIPT_TEMPLATE: &str = r#"[
  [{"type":"ToolCall","id":"c1","name":"edit","input":{"path":"__PATH__","old_string":"let a = 1;","new_string":"let a = ONE;"}}],
  [{"type":"Text","text":"done"}]
]"#;

fn edit_script(rel_path: &str) -> String {
    EDIT_SCRIPT_TEMPLATE.replace("__PATH__", rel_path)
}

/// A scripted edit drives the real edit tool through the binary; the tool
/// returns a result with a top-level diff field, the transcript marks it
/// is_diff, and the structured diff renders end-to-end (the "Added 1 line,
/// removed 1 line" summary + the line-numbered add/remove rows). Pins the real
/// render path the unit count==render tests cannot reach, and is the first
/// instance of the (d) generic interaction-verification infra — a real tool
/// result rendered + scrolled under PTY, where a stashed-width vs actual-width
/// mismatch would drift the scroll offset past real rows.
#[test]
#[ignore]
fn test_edit_diff_renders_structured() {
    let repo = make_temp_repo(1);
    let fixture = Fixture::new(&repo);
    let script = edit_script(&fixture.rel());
    let mut s = session_on_working_in_repo(repo, &script);
    s.send_str("edit the file");
    s.send_key(&Key::Enter);
    // The structured-diff summary rendered. The full "Added 1 line, removed 1
    // line" is split by ANSI bold codes around the count digits ([1m1[22m),
    // so a raw-substring assert across the digits would miss; assert the two
    // contiguous tokens instead — together they pin the summary, which plain
    // stdout or a fold would not produce.
    assert!(
        s.wait_for("Added", RENDER_TIMEOUT),
        "the edit diff summary head should render:\n{}",
        s.output()
    );
    assert!(
        s.wait_for("removed", RENDER_TIMEOUT),
        "the edit diff summary tail should render:\n{}",
        s.output()
    );
    // The added line's new content rendered. "ONE" is contiguous in the raw
    // stream (the word-diff bg color wraps the whole word, so the word itself
    // is unbroken); "let a = ONE;" as a whole is split by that same color code
    // around "ONE", so assert the word alone — it only appears in the added
    // diff row, so it discriminates the structured add from the remove + from
    // a plain-stdout render.
    assert!(
        s.wait_for("ONE", RENDER_TIMEOUT),
        "the added diff line content should render:\n{}",
        s.output()
    );
    // The run ended on the second scripted response.
    assert!(
        s.wait_for("done", RENDER_TIMEOUT),
        "the run should end after the edit:\n{}",
        s.output()
    );
    // The scripted provider drove the run (not the default stub fallback that
    // prints "stub mode: no api key resolved").
    assert!(
        !s.output().contains("stub mode: no api key"),
        "the scripted provider should drive the run, not the default stub:\n{}",
        s.output()
    );
}
