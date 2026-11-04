//! Tests for this module.

use super::*;
use houyicoder_memory::InMemoryBackend;

fn journal() -> Arc<FileTransaction> {
    Arc::new(FileTransaction::new(Arc::new(InMemoryBackend::new())))
}

/// A host carries the write journal so a reattached connection lists the
/// writes the detached run made, and reports none when it was built without
/// one.
#[test]
fn test_host_carries_journal() {
    let bare = SessionHost::new(SessionLeaseStore::new());
    assert!(
        bare.file_transaction().is_none(),
        "a host built without a journal reports none"
    );
    let host = SessionHost::new(SessionLeaseStore::new()).with_file_transaction(Some(journal()));
    assert!(
        host.file_transaction().is_some(),
        "the attached journal survives into the reattach path"
    );
}
