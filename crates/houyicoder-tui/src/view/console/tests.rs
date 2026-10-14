use super::*;

#[test]
fn test_console_uses_shared_verdict() {
    // The console delegates to the shared component, so the real verdict
    // is Cyan (not Red) under the monochrome vocabulary.
    assert_eq!(components::verdict_style("real").fg, Some(Color::Cyan));
    assert_eq!(
        components::verdict_style("refuted").fg,
        Some(Color::DarkGray)
    );
}
