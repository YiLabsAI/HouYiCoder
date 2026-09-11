use super::assemble_instructions;

#[test]
fn test_empty_configured_uses_assembled() {
    // Empty configured field returns the assembled system prompt unchanged.
    let assembled = "identity\n\nsystem";
    let out = assemble_instructions(assembled, "");
    assert_eq!(out, assembled);
}

#[test]
fn test_nonempty_configured_appends() {
    // A non-empty configured field appends to the assembled system prompt.
    let assembled = "identity\n\nsystem";
    let out = assemble_instructions(assembled, "extra rules");
    assert!(
        out.starts_with("identity"),
        "assembled prefix kept, not replaced"
    );
    assert!(out.contains("extra rules"), "configured text appended");
    assert!(
        out.contains("system") && out.contains("extra rules"),
        "both assembled + configured present"
    );
    assert!(
        out.contains("system\n\nextra rules"),
        "blank-line separator"
    );
}
