use super::*;

#[test]
fn test_large_has_seven_lines() {
    let t = large();
    assert_eq!(t.lines.len(), 7);
}

#[test]
fn test_medium_has_five_lines() {
    let t = medium();
    assert_eq!(t.lines.len(), 5);
}

#[test]
fn test_large_lines_uniform_width() {
    // All large() lines must be exactly width 15 or the strike misaligns
    // from the sun disc.
    for (i, l) in large().lines.iter().enumerate() {
        assert_eq!(
            l.width(),
            15,
            "large line {i} width {} != 15 -> misaligned",
            l.width()
        );
    }
}

#[test]
fn test_medium_lines_uniform_width() {
    for (i, l) in medium().lines.iter().enumerate() {
        assert_eq!(
            l.width(),
            11,
            "medium line {i} width {} != 11 -> misaligned",
            l.width()
        );
    }
}

#[test]
fn test_large_strike_hits_left() {
    // The strike line is line 3. Columns 0-1 are the arrow, col 2 is the
    // strike shaft, col 3 the spark, cols 4-12 the sun, cols 13-14 blank.
    let strike = &large().lines[3];
    let s: String = strike.spans.iter().map(|sp| sp.content.as_ref()).collect();
    assert!(
        s.starts_with("->|*"),
        "strike line must start with the arrow + strike point, got [{s}]"
    );
    assert_eq!(s.len(), 15);
}

#[test]
fn test_wordmark_two_spans() {
    assert_eq!(wordmark().spans.len(), 2);
}

#[test]
fn test_tiny_two_spans() {
    assert_eq!(tiny().len(), 2);
}
