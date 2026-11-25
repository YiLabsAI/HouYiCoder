//! Tests for the URI decoding and the structured noise filter.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::PathBuf;
use std::process::id;

use serde_json::json;

use super::*;

fn tmp_root(tag: &str) -> PathBuf {
    let root = temp_dir().join(format!("houyi-lsp-render-{tag}-{}", id()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("root");
    root
}

fn location(uri: &str) -> Value {
    json!({"uri": uri,
        "range": {"start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 1}}})
}

/// Percent escapes decode as bytes, so a multi-byte glyph survives intact,
/// and an incomplete or malformed escape is kept as written.
#[test]
fn test_uri_decodes_path() {
    assert_eq!(uri_to_path("file:///w/a%20b.rs"), "/w/a b.rs");
    assert_eq!(uri_to_path("file:///w/%E6%B5%8B%E8%AF%95.rs"), "/w/测试.rs");
    assert_eq!(uri_to_path("file:///w/测试.rs"), "/w/测试.rs");
    assert_eq!(uri_to_path("file:///w/a%2.rs"), "/w/a%2.rs");
    assert_eq!(uri_to_path("file:///w/a%zz.rs"), "/w/a%zz.rs");
    assert_eq!(uri_to_path("file:///c:/w/a.rs"), "c:/w/a.rs");
}

/// Locations inside the standard noise directories and the repository's own
/// ignore rules are dropped before they are rendered; the kept rows survive
/// unchanged.
#[test]
fn test_noise_filtered() {
    let root = tmp_root("noise");
    write(root.join(".gitignore"), "generated/\n").expect("ignore");
    let filter = NoiseFilter::new(&root);
    let generated = format!("file://{}/generated/a.rs", root.display());
    let kept = json!([
        location("file:///w/target/debug/a.rs"),
        location(&generated),
        location("file:///w/node_modules/x.js"),
        location("file:///w/vendor/probe/a.rs"),
        location("file:///w/src/a.rs")
    ]);
    assert_eq!(locations_text(&kept, &filter), "/w/src/a.rs:1:1");
    assert_eq!(locations_text(&json!(null), &filter), "");
    drop(remove_dir_all(&root));
}

/// A workspace symbol whose file sits in a skipped directory never reaches
/// the answer, and a call-hierarchy item follows the same rule.
#[test]
fn test_symbol_rows_skip_noise() {
    let filter = NoiseFilter::new(&temp_dir());
    let symbols: Vec<LspSymbolInformation> = serde_json::from_value(json!([
        {"name": "hidden", "kind": 12, "location": location("file:///w/target/a.rs")},
        {"name": "shown", "kind": 12, "location": location("file:///w/src/a.rs")}
    ]))
    .expect("parses");
    assert_eq!(
        symbol_list_text(&symbols, &filter),
        "shown: /w/src/a.rs:1:1"
    );
    let items: Vec<LspCallHierarchyItem> = serde_json::from_value(json!([
        {"name": "hidden", "kind": 12, "uri": "file:///w/.git/a.rs",
            "range": {"start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 1}}},
        {"name": "shown", "kind": 12, "uri": "file:///w/a.rs",
            "range": {"start": {"line": 2, "character": 4},
                "end": {"line": 2, "character": 9}}}
    ]))
    .expect("parses");
    assert_eq!(hierarchy_items_text(&items, &filter), "shown (/w/a.rs:3:5)");
}
