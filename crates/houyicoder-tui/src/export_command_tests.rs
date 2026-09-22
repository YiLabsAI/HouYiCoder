#![cfg(test)]
//! /export command tests: the handler's branches (disconnected, capability
//! gap, successful write, write failure) + the string-dispatch path that
//! routes an export with a path arg to the handler. Covers run_export
//! end-to-end at the unit level (the PTY test pins the real-binary path;
//! these pin the branches the PTY run does not reliably hit).

use std::sync::Arc;

use crate::state::{App, TranscriptLine};
use crate::view::export_log::{ExportLog, ExportPayload};

/// A stub ExportLog that returns a fixed payload so run_export's write path is
/// testable without a real SessionLog.
struct StubExport {
    payload: ExportPayload,
}

impl ExportLog for StubExport {
    fn export(&self) -> ExportPayload {
        ExportPayload {
            filename: self.payload.filename.clone(),
            json: self.payload.json.clone(),
        }
    }
}

fn last_system_line(app: &App) -> Option<String> {
    app.transcript.iter().rev().find_map(|l| match l {
        TranscriptLine::System(s) => Some(s.clone()),
        _ => None,
    })
}

#[test]
fn test_export_writes_file_reports() {
    let dir = std::env::temp_dir().join(format!(
        "houyi-export-unit-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("out.json");
    let mut app = crate::test_harness::connected_app();
    app.export_log = Some(Arc::new(StubExport {
        payload: ExportPayload {
            filename: "ignored.json".into(),
            json: "{\"hello\":\"world\"}".into(),
        },
    }));
    app.run_export(Some(target.to_str().unwrap()));
    let line = last_system_line(&app).expect("a system line lands");
    assert!(
        line.contains("export: wrote"),
        "expected 'export: wrote', got: {line}"
    );
    assert!(target.exists(), "the file must land on disk");
    let body = std::fs::read_to_string(&target).unwrap();
    assert!(body.contains("hello"), "the serialized JSON is in the file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "export file is 0o600");
    }
    std::fs::remove_file(&target).ok();
    std::fs::remove_dir(&dir).ok();
}

#[test]
fn test_export_disconnected() {
    let mut app = crate::composition::app();
    // session stays None (the default) — the disconnected branch.
    app.run_export(None);
    let line = last_system_line(&app).expect("a system line lands");
    assert!(
        line.contains("export: not connected"),
        "expected the disconnected report, got: {line}"
    );
}

#[test]
fn test_export_source_gap() {
    let mut app = crate::test_harness::connected_app();
    // Connected, but the export source is not installed — a capability gap,
    // not a disconnect.
    app.run_export(None);
    let line = last_system_line(&app).expect("a system line lands");
    assert!(
        line.contains("export: unavailable in this session"),
        "expected the capability report, got: {line}"
    );
}

#[test]
fn test_unwritable_path_reports_error() {
    let mut app = crate::test_harness::connected_app();
    app.export_log = Some(Arc::new(StubExport {
        payload: ExportPayload {
            filename: "x.json".into(),
            json: "{}".into(),
        },
    }));
    // A path under a directory that does not exist — the atomic write fails.
    let bad = std::env::temp_dir().join("houyi-no-such-dir-xyz/sub/export.json");
    app.run_export(Some(bad.to_str().unwrap()));
    let line = last_system_line(&app).expect("a system line lands");
    assert!(
        line.contains("export: could not write"),
        "expected the failure report, got: {line}"
    );
}

#[test]
fn test_dispatched_via_local_command() {
    // run_tui_local_command with an export + path arg routes to run_export
    // via the arg-parse branch — covers the dispatch arm. Returns true.
    let dir = std::env::temp_dir().join(format!(
        "houyi-export-dispatch-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("dispatched.json");
    let mut app = crate::test_harness::connected_app();
    app.export_log = Some(Arc::new(StubExport {
        payload: ExportPayload {
            filename: "d.json".into(),
            json: "{}".into(),
        },
    }));
    let matched = app.run_tui_local_command(format!("export {}", target.display()).as_str());
    assert!(matched, "the string dispatcher claims /export");
    assert!(target.exists(), "the dispatched command wrote the file");
    assert!(
        last_system_line(&app)
            .map(|s| s.contains("export: wrote"))
            .unwrap_or(false),
        "the dispatched run reported success"
    );
    std::fs::remove_file(&target).ok();
    std::fs::remove_dir(&dir).ok();
}
