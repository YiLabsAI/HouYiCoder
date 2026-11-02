//! The live child registry and the driver slots the shutdown join drains.
//!
//! A child's identity, its lifecycle token, and the mode its ending is
//! published with live together here, next to the driver slot that proves the
//! child is still being driven. Keeping them in one module is what makes the
//! shutdown join able to reason about them as a single unit.

use super::*;

pub(super) struct ChildEntry {
    pub(super) runner: Weak<Runner>,
    pub(super) cancel: CancellationToken,
    /// The mode this child's ending is published with; a foreground child
    /// carries a flag the runtime can flip to hand it to the background.
    pub(super) backgrounded: Option<Arc<AtomicBool>>,
    /// Raised when the reader promotes a running child.
    pub(super) promote: Option<Arc<Notify>>,
}

/// How the shutdown join observes one driver. A background driver's handle
/// is owned solely by the registry. A foreground driver's handle stays with
/// the caller (it awaits the child's outcome), so the registry holds only
/// the abort handle; the completion signal lives on the slot, shared by both
/// modes.
pub(super) enum DriverEntry {
    Background(JoinHandle<()>),
    Foreground { abort: AbortHandle },
}

/// One admitted driver's shutdown credential: the child's lifecycle cancel
/// token (the exact token stored on the child handle) plus the completion
/// signal the join waits on. The entry only exists once attached; before
/// that the close and done pair already covers the child, so a shutdown
/// landing between admission and attach still cancels it and waits for its
/// driver to end.
pub(super) struct DriverSlot {
    pub(super) close: CancellationToken,
    pub(super) done: Receiver<()>,
    pub(super) entry: Option<DriverEntry>,
}

/// What reserve hands back to the caller: the reserved key, the close token
/// to pass to spawn as the child's cancel, and the completion Sender to move
/// into the driver task. done_tx spends only the caller's consecutive
/// synchronous statements before entering the driver, so a cancelled caller
/// never drops it.
pub(super) struct Reservation {
    pub(super) key: u64,
    pub(super) close: CancellationToken,
    pub(super) done_tx: Sender<()>,
}

/// The driver registry with its closing gate. Keys are reserved before the
/// driver task is built (the task needs its key to remove itself) and
/// attached after; both steps refuse once the gate is closed.
#[derive(Default)]
pub(super) struct DriverRegistry {
    pub(super) closing: bool,
    pub(super) next_key: u64,
    pub(super) drivers: HashMap<u64, DriverSlot>,
    /// The drivers the close step drained, waiting for the join to take.
    pub(super) draining: Vec<(u64, DriverSlot)>,
}

impl DriverRegistry {
    /// Reserve a driver's admission credential before its first side effect.
    /// Creates the completion channel so close and done are both live from
    /// this moment, before the child exists. None once the gate is closed: a
    /// shutdown in progress refuses new children at the door.
    pub(super) fn reserve(&mut self, close: CancellationToken) -> Option<Reservation> {
        if self.closing {
            return None;
        }
        let key = self.next_key;
        self.next_key += 1;
        let (done_tx, done) = channel::<()>();
        self.drivers.insert(
            key,
            DriverSlot {
                close: close.clone(),
                done,
                entry: None,
            },
        );
        Some(Reservation {
            key,
            close,
            done_tx,
        })
    }

    /// Attach a spawned driver under its reserved key. Err hands the entry
    /// back once the slot is gone (a shutdown drained it): the caller must
    /// then join the driver itself. The slot's presence is the whole answer,
    /// so this no longer reads closing separately.
    pub(super) fn attach(&mut self, key: u64, entry: DriverEntry) -> Result<(), DriverEntry> {
        match self.drivers.get_mut(&key) {
            Some(slot) => {
                slot.entry = Some(entry);
                Ok(())
            }
            None => Err(entry),
        }
    }

    /// The driver finished; drop its entry so a later join waits only for
    /// still-running drivers.
    pub(super) fn remove(&mut self, key: u64) {
        self.drivers.remove(&key);
    }

    /// Close the gate and drain the registered drivers into the taken state,
    /// where they wait for take_drained. The caller holds this lock across
    /// the children cancel pass, which is what makes a registration racing
    /// the shutdown see either a cancelled token or a closed gate — never
    /// neither. The drain fills only when empty, so a repeated close cannot
    /// discard a prior drain the join has not taken yet; the single-close
    /// consumer stays the boundary.
    pub(super) fn close_and_drain_into(&mut self) {
        self.closing = true;
        if self.draining.is_empty() {
            self.draining = take(&mut self.drivers).into_iter().collect();
        }
    }

    /// Take the drivers the close step drained. Empty when the gate never
    /// closed (an empty registry at shutdown) or after a prior take.
    pub(super) fn take_drained(&mut self) -> Vec<(u64, DriverSlot)> {
        take(&mut self.draining).into_iter().collect()
    }
}
