//! The agent registry the composition root installs: the built-in
//! definitions, layered with the user and project directories on disk.

use std::path::Path;
use std::sync::Arc;

use houyicoder_core::agent::multi_agent::loader::{DefinitionSource, Loader, load_dir};
use houyicoder_core::agent::multi_agent::registry::AgentRegistry;
use houyicoder_core::agent::multi_agent::registry::BuiltInRegistry;
use houyicoder_core::agent::multi_agent::registry::built_in_all;

/// The built-in agent registry: the five built-in agent definitions. Shared
/// between the agent tool (resolve) and the spawn runtime (materialize) so
/// the two see one source of truth.
pub(super) fn built_in_registry() -> Arc<dyn AgentRegistry> {
    Arc::new(BuiltInRegistry::from_agents(built_in_all()))
}

/// Load the agent registry from disk: the built-in definitions, the user
/// level, and the project level, with a later layer overriding an earlier
/// one for the same type. The user level reads the home passed in for both
/// spellings; the project level reads the workspace. A directory that does
/// not exist scans as empty, and a corrupt file is skipped rather than
/// blanking its layer — one bad definition must not take the registry down.
pub(crate) fn load_agent_registry(
    workspace: Option<&Path>,
    home: Option<&Path>,
) -> Arc<dyn AgentRegistry> {
    let mut loader = Loader::new();
    for def in built_in_all() {
        loader.add(DefinitionSource::BuiltIn, def);
    }
    if let Some(home) = home {
        for dir_name in [".houyicoder/agents", ".claude/agents"] {
            let dir = home.join(dir_name);
            let (defs, _failures) = load_dir(&dir, DefinitionSource::User);
            for (_, def) in defs {
                loader.add(DefinitionSource::User, def);
            }
        }
    }
    if let Some(workspace) = workspace {
        for dir_name in [".houyicoder/agents", ".claude/agents"] {
            let dir = workspace.join(dir_name);
            let (defs, _failures) = load_dir(&dir, DefinitionSource::Project);
            for (_, def) in defs {
                loader.add(DefinitionSource::Project, def);
            }
        }
    }
    Arc::new(BuiltInRegistry::from_agents(loader.merge()))
}
