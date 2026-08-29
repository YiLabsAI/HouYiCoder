//! Bus ACL: deny-by-default subscription control. The bus transport is
//! dumb (carries any message to any subscriber); this layer enforces who
//! can subscribe to whose topics (progress/completed: parent only; inbox:
//! publishers = parent only, so a child cannot spoof another's inbox).
//! Status: implemented + unit-tested but not yet wired into the bus — the
//! transport does not consult these policies, so deny-by-default is not
//! enforced end-to-end. Defer to a wiring task.

use std::collections::HashSet;

/// A subscription access control list. Deny by default: only the
/// agent IDs in the allowed set can subscribe to the topic.
#[derive(Debug, Clone)]
pub struct SubscribeAcl {
    /// The agent IDs allowed to subscribe. Empty means nobody can
    /// (deny-by-default); the parent + orchestrator are added at
    /// spawn time.
    allowed: HashSet<String>,
}

impl SubscribeAcl {
    /// Create a deny-all ACL. Nobody can subscribe until explicitly
    /// added.
    pub fn deny_all() -> Self {
        Self {
            allowed: HashSet::new(),
        }
    }

    /// Create an ACL that allows only the given agent IDs.
    pub fn allow_only(ids: &[&str]) -> Self {
        Self {
            allowed: ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Add an agent to the allowed set.
    pub fn allow(&mut self, agent_id: &str) {
        self.allowed.insert(agent_id.to_string());
    }

    /// Check whether an agent is allowed to subscribe.
    pub fn is_allowed(&self, agent_id: &str) -> bool {
        self.allowed.contains(agent_id)
    }
}

impl Default for SubscribeAcl {
    fn default() -> Self {
        Self::deny_all()
    }
}

/// The policy for a topic: who can subscribe and who can publish.
///
/// - progress/completed: subscribers = parent + orchestrator;
///   publisher = the child that owns the topic.
/// - inbox: publisher = parent + orchestrator only; a child cannot
///   publish into another child's inbox.
#[derive(Debug, Clone)]
pub struct TopicPolicy {
    /// Who can subscribe to this topic.
    pub subscribers: SubscribeAcl,
    /// Who can publish to this topic.
    pub publishers: SubscribeAcl,
}

impl TopicPolicy {
    /// Policy for a child's progress topic: the child publishes,
    /// the parent + orchestrator subscribe.
    pub fn progress(parent_id: &str, orchestrator_id: Option<&str>) -> Self {
        let mut subs = SubscribeAcl::allow_only(&[parent_id]);
        if let Some(orch) = orchestrator_id {
            subs.allow(orch);
        }
        let pubs = SubscribeAcl::deny_all();
        Self {
            subscribers: subs,
            publishers: pubs,
        }
    }

    /// Policy for a child's completed topic: same as progress.
    pub fn completed(parent_id: &str, orchestrator_id: Option<&str>) -> Self {
        Self::progress(parent_id, orchestrator_id)
    }

    /// Policy for a child's inbox topic: parent + orchestrator
    /// publish, nobody subscribes except the child itself (the child
    /// owns the inbox receiver, not a bus subscriber).
    pub fn inbox(parent_id: &str, orchestrator_id: Option<&str>) -> Self {
        let mut pubs = SubscribeAcl::allow_only(&[parent_id]);
        if let Some(orch) = orchestrator_id {
            pubs.allow(orch);
        }
        Self {
            subscribers: SubscribeAcl::deny_all(),
            publishers: pubs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deny_all_blocks_everyone() {
        let acl = SubscribeAcl::deny_all();
        assert!(!acl.is_allowed("alice"));
        assert!(!acl.is_allowed("bob"));
    }

    #[test]
    fn test_allow_only_permits_listed() {
        let acl = SubscribeAcl::allow_only(&["alice", "bob"]);
        assert!(acl.is_allowed("alice"));
        assert!(acl.is_allowed("bob"));
        assert!(!acl.is_allowed("eve"));
    }

    #[test]
    fn test_allow_adds() {
        let mut acl = SubscribeAcl::deny_all();
        acl.allow("alice");
        assert!(acl.is_allowed("alice"));
        assert!(!acl.is_allowed("bob"));
    }

    /// A child cannot subscribe to another child's progress: only
    /// the parent + orchestrator are allowed.
    #[test]
    fn test_progress_blocks_others() {
        let policy = TopicPolicy::progress("parent-1", None);
        assert!(policy.subscribers.is_allowed("parent-1"));
        assert!(!policy.subscribers.is_allowed("child-a"));
        assert!(!policy.subscribers.is_allowed("child-b"));
    }

    /// The parent can publish to a child's inbox; another child
    /// cannot.
    #[test]
    fn test_inbox_blocks_others() {
        let policy = TopicPolicy::inbox("parent-1", None);
        assert!(policy.publishers.is_allowed("parent-1"));
        assert!(!policy.publishers.is_allowed("child-a"));
    }

    /// Orchestrator is allowed when provided.
    #[test]
    fn test_orchestrator_allowed() {
        let policy = TopicPolicy::progress("parent-1", Some("orch-1"));
        assert!(policy.subscribers.is_allowed("parent-1"));
        assert!(policy.subscribers.is_allowed("orch-1"));
        assert!(!policy.subscribers.is_allowed("child-a"));
    }
}
