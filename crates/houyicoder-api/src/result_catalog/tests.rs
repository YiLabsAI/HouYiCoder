//! Tests for the result catalog error's wording.

use super::ResultCatalogError;

/// Every refusal a reader can hand back says which thing went wrong and keeps
/// the detail the catalog supplied, because the reader decides what to do next
/// from that sentence rather than from the variant alone.
#[test]
fn test_error_wording_names_cause() {
    let cases = [
        (ResultCatalogError::NotFound, "no such result"),
        (
            ResultCatalogError::InvalidCursor("stale".into()),
            "cursor not accepted: stale",
        ),
        (
            ResultCatalogError::InvalidRange("past the end".into()),
            "range refused: past the end",
        ),
        (
            ResultCatalogError::UnsupportedRead("no windows".into()),
            "windowed read is not available: no windows",
        ),
        (
            ResultCatalogError::Corrupt("hash mismatch".into()),
            "stored result is corrupt: hash mismatch",
        ),
        (
            ResultCatalogError::Store("log locked".into()),
            "session log read failed: log locked",
        ),
    ];
    for (error, expected) in cases {
        let text = error.to_string();
        assert!(
            text.contains(expected),
            "wording for {error:?} names the cause: {text}"
        );
    }
}
