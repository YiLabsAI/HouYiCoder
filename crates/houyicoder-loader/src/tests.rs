//! The loader binary: argument handling and the load report it prints.

use super::*;
use std::env::temp_dir;
use std::fs::File;
use std::fs::create_dir_all;
use std::fs::read_to_string;
use std::fs::write;
use std::io::BufRead;
use std::io::BufReader;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};

fn write_cc_log(path: &Path, lines: &[&str]) {
    write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn test_export_log_produces_sessions() {
    let tmp = tempfile_dir();
    let cc = tmp.join("cc.jsonl");
    write_cc_log(
        &cc,
        &[
            r#"{"type":"user","isMeta":false,"sessionId":"11111111-1111-1111-1111-111111111111","cwd":"/repo","timestamp":"2026-08-02T15:38:23.123Z","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","isMeta":false,"sessionId":"11111111-1111-1111-1111-111111111111","cwd":"/repo","timestamp":"2026-08-02T15:38:24.000Z","message":{"role":"assistant","model":"glm-5.2","content":[{"type":"text","text":"hello back"}]}}"#,
            r#"{"type":"mode","sessionId":"11111111-1111-1111-1111-111111111111"}"#,
        ],
    );
    let out = tmp.join("sessions");
    let cfg = Cfg {
        cc_path: cc.to_string_lossy().into_owned(),
        out_dir: out.to_string_lossy().into_owned(),
    };
    run(&cfg).unwrap();

    // Output log + descriptor exist.
    let sid = "11111111-1111-1111-1111-111111111111";
    let log = out.join(sid).join("log.jsonl");
    let descriptor = out.join(sid).join("session.json");
    assert!(log.exists(), "log.jsonl written");
    assert!(descriptor.exists(), "session.json written");

    // Descriptor carries the model + cwd.
    let sc: serde_json::Value =
        serde_json::from_str(&read_to_string(&descriptor).unwrap()).unwrap();
    assert_eq!(sc["model"], "glm-5.2");
    assert_eq!(sc["cwd"], "/repo");

    // Chain verifies: each line's prev_hash = SHA-256 of the previous
    // line's bytes (the first is null). The mode line is skipped.
    let lines: Vec<String> = BufRead::lines(BufReader::new(File::open(&log).unwrap()))
        .map(|l| l.unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "user + assistant events (mode skipped)");
    let mut prev: Option<PrevHash> = None;
    for line in &lines {
        let ev: houyicoder_context::SessionLogEntry = serde_json::from_str(line).unwrap();
        assert_eq!(ev.prev_hash, prev, "chain links at {}", line.len());
        let mut h = Sha256::new();
        h.update(line.as_bytes());
        let b: [u8; 32] = h.finalize().as_slice().try_into().unwrap();
        prev = Some(PrevHash(b));
    }
}

fn tempfile_dir() -> PathBuf {
    let mut p = temp_dir();
    p.push(format!("houyi-load-test-{}", id()));
    p.push(format!("{}", random_suffix()));
    create_dir_all(&p).unwrap();
    p
}

fn random_suffix() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}
