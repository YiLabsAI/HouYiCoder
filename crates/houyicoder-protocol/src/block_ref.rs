//! The marker an output carries when its bytes live in the block store.
//!
//! The append stage writes the marker, the retention planner decides how to
//! serve it, and the reclaim pass walks it to learn which blocks a live log
//! still needs. All three read the vocabulary from here, so a producer and a
//! reader cannot drift onto different spellings of the same marker.

use serde_json::Value;

/// The key under which an output names the block that holds its bytes.
pub const BLOCK_REF_KEY: &str = "block_ref";

/// The key under which a whole-output marker restates every address the
/// object it replaced named, so the walk still reaches them.
pub const CONTAINS_KEY: &str = "contains";

/// The key under which a marker names the stored byte length of its block.
pub const BLOCK_BYTES_KEY: &str = "bytes";

/// The key under which a marker carries the truncated inline copy of the
/// content it stands in for.
pub const PREVIEW_KEY: &str = "preview";

/// The key under which a served marker tells the model how to reach the
/// content it left out.
pub const HINT_KEY: &str = "hint";

/// Every block address a value names, in document order.
///
/// A marker is an object carrying the address under the marker key, and the
/// producer places it either at the top level of an output (the whole output
/// externalized) or as the value of one field (a single large field of an
/// envelope externalized). The walk follows the marker wherever it sits.
///
/// A whole-output externalize replaces the object that named other blocks, so
/// that marker restates every address the replaced object carried under the
/// contains key. The reclaim pass never opens stored bytes; an address that
/// survived only inside the stored copy would read as unreferenced and be
/// released while the log still needs it.
pub fn block_addresses(value: &Value) -> Vec<&str> {
    block_sizes(value)
        .into_iter()
        .map(|(address, _)| address)
        .collect()
}

/// Every block address a value names, with the stored length its own marker
/// reports when it reports one.
///
/// The address and the size sit on the same marker object, so one walk
/// answers both the reclaim pass (which needs addresses) and the readers
/// that gate on size (a budget pre-check, a row tag) without each of them
/// repeating the traversal and the key spelling.
pub fn block_sizes(value: &Value) -> Vec<(&str, Option<u64>)> {
    let mut out = Vec::new();
    collect(value, &mut out);
    out
}

fn collect<'a>(value: &'a Value, out: &mut Vec<(&'a str, Option<u64>)>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == BLOCK_REF_KEY {
                    // A marker whose address is not a string names no block.
                    // Skipping it keeps a stored value of the wrong shape
                    // from failing the whole walk.
                    if let Some(raw) = child.as_str() {
                        out.push((raw, map.get(BLOCK_BYTES_KEY).and_then(Value::as_u64)));
                    }
                } else {
                    collect(child, out);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
