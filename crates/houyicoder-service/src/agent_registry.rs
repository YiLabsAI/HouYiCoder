//! Agent registry assembly for the composition root.
//!
//! Merges the built-in roster with user- and project-layer definition files
//! (an agents/ directory of .md) by precedence: built-in < user < project,
//! later layers overriding earlier ones per subagent_type. The merged
//! registry is the Arc<dyn AgentRegistry> the runtime and the agent tool
//! consult.
//!
//! Model-tier tokens (Flash/Max) and permission-mode tokens on the
//! definitions resolve to concrete ids and permission modes at spawn time,
//! where config and the permission crate are available; this assembly only
//! layers sources.

use std::path::Path;
use std::sync::Arc;

use houyicoder_core::agent::multi_agent::loader::{DefinitionSource, Loader, load_dir};
use houyicoder_core::agent::multi_agent::registry::{AgentRegistry, BuiltInRegistry, built_in_all};

/// Build the registry from the built-in roster plus optional user and
/// project agent directories. Project overrides user, user overrides
/// built-in, per subagent_type.
pub fn build_agent_registry(
    user_agents_dir: Option<&Path>,
    project_agents_dir: Option<&Path>,
) -> Arc<dyn AgentRegistry> {
    let mut loader = Loader::new();
    for def in built_in_all() {
        loader.add(DefinitionSource::BuiltIn, def);
    }
    if let Some(dir) = user_agents_dir {
        for (source, def) in load_dir(dir, DefinitionSource::User).0 {
            loader.add(source, def);
        }
    }
    if let Some(dir) = project_agents_dir {
        for (source, def) in load_dir(dir, DefinitionSource::Project).0 {
            loader.add(source, def);
        }
    }
    Arc::new(BuiltInRegistry::from_agents(loader.merge()))
}

#[cfg(test)]
mod tests;
