use super::*;

#[test]
fn test_exec_tools_map_exec() {
    for n in ["bash", "sh", "exec", "shell"] {
        assert_eq!(side_effect_for(n), SideEffect::Exec, "{n}");
    }
}

#[test]
fn test_edit_tools_map_filesystem() {
    for n in ["write", "edit", "multiedit", "patch"] {
        assert_eq!(side_effect_for(n), SideEffect::Filesystem, "{n}");
    }
}

#[test]
fn test_fetch_tools_map_network() {
    for n in ["webfetch", "netfetch", "fetch"] {
        assert_eq!(side_effect_for(n), SideEffect::Network, "{n}");
    }
}

#[test]
fn test_read_tools_map_none() {
    for n in ["read", "view", "ls", "grep"] {
        assert_eq!(side_effect_for(n), SideEffect::None, "{n}");
    }
}

#[test]
fn test_unknown_maps_to_none() {
    assert_eq!(side_effect_for("unknown_tool"), SideEffect::None);
}

#[test]
fn test_case_insensitive_mapping() {
    assert_eq!(side_effect_for("BASH"), SideEffect::Exec);
    assert_eq!(side_effect_for("Edit"), SideEffect::Filesystem);
}
