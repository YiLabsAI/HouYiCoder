use super::*;

#[test]
fn test_default_closed_empty() {
    let p = PaletteState::default();
    assert!(!p.open);
    assert!(p.query.is_empty());
    assert_eq!(p.sel, 0);
}

#[test]
fn test_open_resets_palette_state() {
    let mut p = PaletteState::default();
    p.query.push('x');
    p.sel = 3;
    p.open();
    assert!(p.open);
    assert!(p.query.is_empty());
    assert_eq!(p.sel, 0);
}

#[test]
fn test_seed_query_for_argument() {
    let mut p = PaletteState::default();
    p.open();
    p.seed_query("permissions");
    assert_eq!(p.query, "permissions ", "the name plus a trailing space");
    assert_eq!(p.sel, 0, "the selection resets to the top");
    assert!(p.open, "the popup stays open for the argument");
}

#[test]
fn test_filter_narrows_then_resets() {
    let mut p = PaletteState::default();
    p.open();
    let full = p.len();
    assert_eq!(full, SlashCommand::ALL.len());
    p.push('s');
    p.push('p');
    p.push('e');
    assert!(p.len() < full);
    let cmd = p.selected().expect("filtered list non-empty");
    assert!(cmd.name().contains("spe"));
}

#[test]
fn test_prefix_ranks_before_substring() {
    let mut p = PaletteState::default();
    p.open();
    p.push('a');
    assert_eq!(p.selected(), Some(SlashCommand::Agents));
    let names: Vec<&str> = p.filtered().iter().map(SlashCommand::name).collect();
    let agents = names.iter().position(|name| *name == "/agents").unwrap();
    let clear = names.iter().position(|name| *name == "/clear").unwrap();
    assert!(agents < clear, "prefix matches must precede substrings");
}
