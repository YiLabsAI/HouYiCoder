//! Tests for this module.

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
