//! The marker an output carries when its bytes live in the block store.
//!
//! The append stage writes the marker, the retention planner decides how to
//! serve it, and the reclaim pass walks it to learn which blocks a live log
//! still needs. All three read the vocabulary from here, so a producer and a
//! reader cannot drift onto different spellings of the same marker.

use serde_json::Value;

/// The key under which an output names the block that holds its bytes.
pub const BLOCK_REF_KEY: &str = "block_ref";

/// Every block address a value names, in document order.
///
/// A marker is an object carrying the address under the marker key, and the
/// producer places it either at the top level of an output (the whole output
/// externalized) or as the value of one field (a single large field of an
/// envelope externalized). The walk follows the marker wherever it sits.
pub fn block_addresses(value: &Value) -> Vec<&str> {
    let mut out = Vec::new();
    collect(value, &mut out);
    out
}

fn collect<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == BLOCK_REF_KEY {
                    // A marker whose address is not a string names no block.
                    // Skipping it keeps a stored value of the wrong shape
                    // from failing the whole walk.
                    if let Some(raw) = child.as_str() {
                        out.push(raw);
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
