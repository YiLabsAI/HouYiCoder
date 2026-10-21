//! Maps a tool name to its side-effect level. The SideEffect enum is defined
//! in the api sandbox port (the layer both the gate and the fence share).

use houyicoder_api::sandbox::SideEffect;

/// Whether a tool name runs a shell command. The shell family shares one
/// command field and one set of command checks.
pub(crate) fn is_shell_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "bash" | "sh" | "exec" | "shell"
    )
}

/// Map a tool name to its side-effect level. Unknown tools default to None —
/// the side-effect is a hint, not a fence; the mode default and the tool's own
/// requires_approval flag still gate unknown tools.
pub fn side_effect_for(tool_name: &str) -> SideEffect {
    match tool_name.to_ascii_lowercase().as_str() {
        "bash" | "sh" | "exec" | "shell" => SideEffect::Exec,
        "write" | "edit" | "multiedit" | "patch" | "str_replace" => SideEffect::Filesystem,
        "webfetch" | "netfetch" | "fetch" | "curl" | "wget" => SideEffect::Network,
        "read" | "view" | "ls" | "grep" | "glob" | "find" | "stat" => SideEffect::None,
        _ => SideEffect::None,
    }
}

#[cfg(test)]
mod tests;
