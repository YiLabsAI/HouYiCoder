//! Maps a tool name to its side-effect level. The SideEffect enum is defined
//! in the api sandbox port (the layer both the gate and the fence share), and
//! the shell and file families with it, so the gate and the engine read one
//! answer about which names belong to either.

use houyicoder_api::sandbox::SideEffect;
use houyicoder_api::tool::{is_shell_tool, writes_files};

/// Map a tool name to its side-effect level. Unknown tools default to None —
/// the side-effect is a hint, not a fence; the mode default reads the tool's
/// own read-only, destructive, and requires-approval flags to gate a name the
/// table leaves unclassified.
pub fn side_effect_for(tool_name: &str) -> SideEffect {
    if is_shell_tool(tool_name) {
        return SideEffect::Exec;
    }
    if writes_files(tool_name) {
        return SideEffect::Filesystem;
    }
    match tool_name.to_ascii_lowercase().as_str() {
        "webfetch" | "netfetch" | "fetch" | "curl" | "wget" => SideEffect::Network,
        "read" | "view" | "ls" | "grep" | "glob" | "find" | "stat" => SideEffect::None,
        _ => SideEffect::None,
    }
}

#[cfg(test)]
mod tests;
