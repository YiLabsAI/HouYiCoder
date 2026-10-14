//! load: convert a session transcript (JSONL: sessionId, parentUuid,
//! isMeta, type: user/assistant) into a session directory this engine
//! can resume.
//!
//! Reads a transcript jsonl (typed records chained by parentUuid)
//! and writes a session directory: log.jsonl with a fresh SHA-256
//! prev_hash chain that the runtime can resume, plus a session.json
//! descriptor carrying the model + cwd. This lets the runtime resume
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

use houyicoder_context::{NameSource, PrevHash, SessionDescriptor, SessionId, SessionProvenance};
use sha2::{Digest, Sha256};
use std::env::args;
use std::env::current_dir;
use std::error::Error;
use std::fs::write;
use std::process::exit;
use std::str::from_utf8;

mod mapping;

struct Cfg {
    cc_path: String,
    out_dir: String,
}

fn main() {
    let cfg = parse_args();
    if let Err(e) = run(&cfg) {
        eprintln!("houyi-load: {e}");
        exit(1);
    }
}

fn parse_args() -> Cfg {
    let args: Vec<String> = args().collect();
    if args.len() != 3 {
        eprintln!("usage: houyi-load <transcript.jsonl> <sessions-dir>");
        exit(2);
    }
    Cfg {
        cc_path: args[1].clone(),
        out_dir: args[2].clone(),
    }
}

fn run(cfg: &Cfg) -> Result<(), Box<dyn Error>> {
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
        let trimmed = from_utf8(&bytes).unwrap_or("").trim_end();
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

    // The source records milliseconds and the descriptor stores seconds, so
    // the conversion happens once, here.
    write_descriptor(cfg, sid, model, cwd, created_at_ms / 1000)?;
    Ok(())
}

/// Find the sessionId on the first parseable record that carries one.
/// Every record carries sessionId, so this is the first line.
fn find_session_id(cc_path: &str) -> Result<SessionId, Box<dyn Error>> {
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

/// Write one SessionLogEntry: set its prev_hash, serialize via serde_json (matching
/// the runtime's write-side hash_event so the chain verifies on re-read),
/// write the line, then return the SHA-256 of the just-written bytes as the
/// next event's prev_hash.
fn write_event(
    writer: &mut BufWriter<File>,
    mut event: houyicoder_context::SessionLogEntry,
    prev_hash: Option<PrevHash>,
) -> Result<Option<PrevHash>, Box<dyn Error>> {
    event.prev_hash = prev_hash;
    let bytes = serde_json::to_vec(&event)?;
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let h: [u8; 32] = hasher.finalize().as_slice().try_into().unwrap();
    Ok(Some(PrevHash(h)))
}

/// Write the session.json descriptor with the captured model + cwd so resume
/// restores them. The cwd falls back to the current dir when the source
/// carried none; the model falls back to a marker string.
fn write_descriptor(
    cfg: &Cfg,
    sid: SessionId,
    model: Option<String>,
    cwd: Option<String>,
    created_at_secs: u64,
) -> Result<(), Box<dyn Error>> {
    let session_dir = Path::new(&cfg.out_dir).join(sid.to_string());
    let descriptor = SessionDescriptor {
        name: None,
        name_source: NameSource::Auto,
        cwd: cwd.filter(|c| !c.is_empty()).unwrap_or_else(|| {
            current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
        model: model
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "imported".to_string()),
        provenance: SessionProvenance::Fresh,
        version: "houyi-load".to_string(),
        created_at: created_at_secs,
        child_session_ids: Vec::new(),
    };
    let json = serde_json::to_string_pretty(&descriptor)?;
    write(session_dir.join("session.json"), json + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests;
