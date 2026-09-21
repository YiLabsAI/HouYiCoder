//! The toggle affordance a foldable row trails: one owner for its wording and
//! for which direction the key currently goes. A row that grows a toggle
//! reads the suffix from here instead of spelling it again, so the vocabulary
//! cannot drift row by row.

/// Which way a foldable row's toggle goes, from the state the row is in now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToggleHint {
    /// The row is collapsed: the key reveals its detail.
    Expand,
    /// The row is expanded: the key hides the detail again.
    Collapse,
}

impl ToggleHint {
    /// The action a group offers now, or none while it is active.
    pub(crate) const fn for_group(active: bool, expanded: bool) -> Option<Self> {
        if active {
            None
        } else if expanded {
            Some(Self::Collapse)
        } else {
            Some(Self::Expand)
        }
    }

    /// The affordance text appended to a foldable row's label, with the
    /// separating space so callers concatenate it directly.
    pub(crate) const fn suffix(self) -> &'static str {
        match self {
            Self::Expand => " (ctrl+o to expand)",
            Self::Collapse => " (ctrl+o to collapse)",
        }
    }
}
