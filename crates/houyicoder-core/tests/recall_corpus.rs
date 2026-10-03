//! Shared corpus loading and seeding for the recall benchmark suites. The
//! fixture carries two hundred topic entries in recency order plus sixty-four
//! query pairs: English lexical, CJK lexical, CJK queries over English
//! descriptions, English paraphrases with no lexical overlap, and negatives.
//!
//! Seeding pins one mtime per entry, a second apart in fixture order, because
//! the rank breaks score ties by recency and files written inside one second
//! would tie in filesystem-hash order, making the candidate window unstable.

use std::env;
use std::fs::{self, FileTimes};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemorySource};
use houyicoder_memory::MarkdownMemoryProvider;
use serde::Deserialize;

/// One topic record: the description is the frontmatter line the lexical
/// rank scores, the body rides along for the write path only.
#[derive(Deserialize)]
pub struct CorpusEntry {
    pub key: String,
    pub description: String,
    pub body: String,
}

/// One benchmark query. The category selects the gate the pair feeds, and
/// expected is the key a correct recall surfaces, absent for negatives.
#[derive(Deserialize)]
pub struct CorpusPair {
    pub category: String,
    pub query: String,
    pub expected: Option<String>,
}

#[derive(Deserialize)]
pub struct Corpus {
    pub entries: Vec<CorpusEntry>,
    pub pairs: Vec<CorpusPair>,
}

impl Corpus {
    /// Every pair in one category, in fixture order.
    pub fn pairs_in(&self, category: &str) -> Vec<&CorpusPair> {
        self.pairs
            .iter()
            .filter(|p| p.category == category)
            .collect()
    }
}

/// Read the fixture that ships beside the tests.
pub fn load_corpus() -> Corpus {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recall_corpus.json");
    let text = fs::read_to_string(&path).expect("corpus fixture readable");
    serde_json::from_str(&text).expect("corpus fixture parses")
}

/// Seed every entry under a fresh root and return the provider over it plus
/// the root for cleanup. Entry content leads with the description line so
/// the write path derives the same frontmatter the fixture declares.
pub fn seed_corpus(corpus: &Corpus) -> (MarkdownMemoryProvider, PathBuf) {
    let root = unique_root("recall-corpus");
    fs::create_dir_all(&root).expect("corpus root creatable");
    let provider = MarkdownMemoryProvider::new(root.clone());
    for entry in &corpus.entries {
        let content = format!("{}\n\n{}", entry.description, entry.body);
        provider
            .add(MemoryEntry::new(
                entry.key.as_str(),
                content,
                MemorySource::Project,
            ))
            .expect("corpus entry seeds");
    }
    pin_recency(&root, corpus);
    (provider, root)
}

/// Best-effort cleanup of a seeded root.
pub fn discard_root(root: &Path) {
    let _removed = fs::remove_dir_all(root);
}

/// Give every topic file a distinct mtime, a second apart in fixture order
/// and safely in the past, so recency tie-breaks are deterministic.
fn pin_recency(root: &Path, corpus: &Corpus) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock readable")
        .as_secs();
    let base = now.saturating_sub(corpus.entries.len() as u64 + 60);
    for (index, entry) in corpus.entries.iter().enumerate() {
        let path = root.join(format!("{}.md", entry.key));
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("topic file openable");
        let modified = UNIX_EPOCH + Duration::from_secs(base + index as u64);
        file.set_times(FileTimes::new().set_modified(modified))
            .expect("mtime pinnable");
    }
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn unique_root(tag: &str) -> PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    env::temp_dir().join(format!("{tag}-{}-{seq}-{nanos}", std::process::id()))
}
