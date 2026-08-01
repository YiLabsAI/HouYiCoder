//! GraphStore — the code-graph query API. Structural queries only.
//! summarize_subtree moved to ContextPlanner (it is an LLM call,
//! not a graph query). Stub interface.

#![allow(dead_code)] // stub graph query trait pending consumer wiring; locally unused

pub struct Symbol {
    pub id: u64,
    pub name: String,
}

pub struct ImpactSet {
    pub symbols: Vec<Symbol>,
}

#[derive(Debug)]
pub enum GraphError {
    NotFound,
    Backend,
}

/// Agent-facing, token-cheap structural queries. No LLM-backed methods here.
pub trait GraphStore: Send + Sync {
    fn definitions_of(&self, sym: &Symbol) -> Result<Vec<Symbol>, GraphError>;
    fn callers_of(&self, sym: &Symbol) -> Result<Vec<Symbol>, GraphError>;
    fn impact_set(&self, sym: &Symbol) -> Result<ImpactSet, GraphError>;
    fn related_tests(&self, sym: &Symbol) -> Result<Vec<Symbol>, GraphError>;
    fn find_symbols(&self, pattern: &str) -> Result<Vec<Symbol>, GraphError>;
}
