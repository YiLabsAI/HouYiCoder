//! load: convert a session transcript (JSONL: sessionId, parentUuid,
//! isMeta, type: user/assistant) into a session directory this engine
//! can resume.
//!
//! Reads a transcript jsonl (typed records chained by parentUuid)
//! and writes a session directory: log.jsonl with a fresh SHA-256
//! prev_hash chain that the runtime can resume, plus a session.json
//! sidecar carrying the model + cwd. This lets the runtime resume
//! sessions from existing transcripts — an interop affordance
//! (transcripts may not resume across version changes; the readable
//! transcript is taken forward here).
//!
//! Usage: houyi-load <transcript.jsonl> <sessions-dir>
//!
//! The output lands at <sessions-dir>/<sid>/log.jsonl + session.json. The sid
//! is reused from the source record's sessionId (a UUID; accepted
//! verbatim). The hash chain is rebuilt fresh (the source's parentUuid
//! linkage is not preserved); the source_chain is Unverified, the rebuilt
//! durable chain is internally self-consistent.

use std::fs::{File, create_dir_all};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use houyicoder_context::{NameSource, PrevHash, SessionId, SessionMeta, SessionProvenance};
use sha2::{Digest, Sha256};

mod mapping;

struct Cfg {
    cc_path: String,
    out_dir: String,
}

fn main() {
    let cfg = parse_args();
    if let Err(e) = run(&cfg) {
        eprintln!("houyi-load: {e}");
        std::process::exit(1);
    }
}

fn parse_args() -> Cfg {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: houyi-load <transcript.jsonl> <sessions-dir>");
        std::process::exit(2);
    }
    Cfg {
        cc_path: args[1].clone(),
        out_dir: args[2].clone(),
    }
}

fn run(cfg: &Cfg) -> Result<(), Box<dyn std::error::Error>> {
    let sid = find_session_id(&cfg.cc_path)?;
    let session_dir = Path::new(&cfg.out_dir).join(sid.to_string());
    create_dir_all(&session_dir)?;
    let log_file = File::create(session_dir.join("log.jsonl"))?;
    let mut writer = BufWriter::new(log_file);

    let mut model: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut created_at_ms: u64 = 0;
    let mut prev_hash: Option<PrevHash> = None;

    let reader = BufReader::new(File::open(&cfg.cc_path)?);
    for line_res in reader.split(b'\n') {
        let bytes = line_res?;
        let trimmed = std::str::from_utf8(&bytes).unwrap_or("").trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue; // lenient: a half-line does not abort the chain
        };
        let ts_ms = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .map(mapping::parse_ts_ms)
            .unwrap_or(0);
        if created_at_ms == 0 {
            created_at_ms = ts_ms;
        }
        for ev in mapping::map_record(&v, sid, ts_ms, &mut model, &mut cwd) {
            prev_hash = write_event(&mut writer, ev, prev_hash)?;
        }
    }
    writer.flush()?;

    write_sidecar(cfg, sid, model, cwd, created_at_ms)?;
    Ok(())
}

/// Find the sessionId on the first parseable record that carries one.
/// Every record carries sessionId, so this is the first line.
fn find_session_id(cc_path: &str) -> Result<SessionId, Box<dyn std::error::Error>> {
    let file = File::open(cc_path)?;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            return Err("no record with a sessionId found".into());
        }
        let trimmed = buf.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
            && let Some(sid_str) = v.get("sessionId").and_then(|s| s.as_str())
        {
            if let Some(sid) = SessionId::from_display_string(sid_str) {
                return Ok(sid);
            }
            return Err(format!("sessionId {sid_str:?} is not a UUID").into());
        }
    }
}

/// Write one TurnEvent: set its prev_hash, serialize via serde_json (matching
/// the runtime's write-side hash_event so the chain verifies on re-read),
/// write the line, then return the SHA-256 of the just-written bytes as the
/// next event's prev_hash.
fn write_event(
    writer: &mut BufWriter<File>,
    mut event: houyicoder_context::TurnEvent,
    prev_hash: Option<PrevHash>,
) -> Result<Option<PrevHash>, Box<dyn std::error::Error>> {
    event.prev_hash = prev_hash;
    let bytes = serde_json::to_vec(&event)?;
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let h: [u8; 32] = hasher.finalize().as_slice().try_into().unwrap();
    Ok(Some(PrevHash(h)))
}

/// Write the session.json sidecar with the captured model + cwd so resume
/// restores them. The cwd falls back to the current dir when the source
/// carried none; the model falls back to a marker string.
fn write_sidecar(
    cfg: &Cfg,
    sid: SessionId,
    model: Option<String>,
    cwd: Option<String>,
    created_at: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_dir = Path::new(&cfg.out_dir).join(sid.to_string());
    let meta = SessionMeta {
        name: None,
        name_source: NameSource::Auto,
        cwd: cwd.filter(|c| !c.is_empty()).unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
        model: model
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "imported".to_string()),
        provenance: SessionProvenance::Fresh,
        version: "houyi-load".to_string(),
        created_at,
        child_session_ids: Vec::new(),
    };
    let json = serde_json::to_string_pretty(&meta)?;
    std::fs::write(session_dir.join("session.json"), json + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_cc_log(path: &Path, lines: &[&str]) {
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
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

        // Output log + sidecar exist.
        let sid = "11111111-1111-1111-1111-111111111111";
        let log = out.join(sid).join("log.jsonl");
        let sidecar = out.join(sid).join("session.json");
        assert!(log.exists(), "log.jsonl written");
        assert!(sidecar.exists(), "session.json written");

        // Sidecar carries the model + cwd.
        let sc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert_eq!(sc["model"], "glm-5.2");
        assert_eq!(sc["cwd"], "/repo");

        // Chain verifies: each line's prev_hash = SHA-256 of the previous
        // line's bytes (the first is null). The mode line is skipped.
        let lines: Vec<String> =
            std::io::BufRead::lines(std::io::BufReader::new(std::fs::File::open(&log).unwrap()))
                .map(|l| l.unwrap())
                .collect();
        assert_eq!(lines.len(), 2, "user + assistant events (mode skipped)");
        let mut prev: Option<PrevHash> = None;
        for line in &lines {
            let ev: houyicoder_context::TurnEvent = serde_json::from_str(line).unwrap();
            assert_eq!(ev.prev_hash, prev, "chain links at {}", line.len());
            let mut h = Sha256::new();
            h.update(line.as_bytes());
            let b: [u8; 32] = h.finalize().as_slice().try_into().unwrap();
            prev = Some(PrevHash(b));
        }
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("houyi-load-test-{}", std::process::id()));
        p.push(format!("{}", random_suffix()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn random_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::SeqCst)
    }
}
