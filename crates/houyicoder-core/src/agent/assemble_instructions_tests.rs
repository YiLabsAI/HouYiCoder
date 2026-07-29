use super::assemble_instructions;

#[test]
fn test_empty_configured_uses_served() {
    // The production path: empty configured field returns the served
    // system prompt unchanged, so the byte-stable prefix is preserved.
    let served = "identity\n\nsystem";
    let out = assemble_instructions(served, "");
    assert_eq!(out, served);
}

#[test]
fn test_nonempty_configured_appends() {
    // A non-empty configured field appends to the served system prompt
    // (the served identity/framework prefix is kept, not replaced).
    let served = "identity\n\nsystem";
    let out = assemble_instructions(served, "extra rules");
    assert!(
        out.starts_with("identity"),
        "served prefix kept, not replaced"
    );
    assert!(out.contains("extra rules"), "configured text appended");
    assert!(
        out.contains("system") && out.contains("extra rules"),
        "both served + configured present"
    );
    // The boundary between them is a blank-line separator.
    assert!(
        out.contains("system\n\nextra rules"),
        "blank-line separator"
    );
}
