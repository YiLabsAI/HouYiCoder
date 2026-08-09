//! Approval cursor-navigation helpers. Split from keys.rs so that file
//! stays under the file-size gate.

/// Display order of the three built-in approval options, the canonical
/// layout: Yes, then Yes-don't-ask, then No. The selected index
/// keeps its internal mapping (0=Yes, 1=No, 2=Yes-don't-ask); this array
/// maps display position to selected value. Used when the server sends no
/// options (the fallback built-in 3-option set).
pub(super) const APPROVAL_DISPLAY_ORDER: [usize; crate::records::APPROVAL_OPTIONS] = [0, 2, 1];

/// The number of options the cursor cycles through: the server's offered
/// list when present, otherwise the built-in set (two when remember is hidden
/// for a protected-path ask, three otherwise).
pub(super) fn option_count(a: &crate::records::Approval) -> usize {
    if a.options.is_empty() {
        a.visible_option_count()
    } else {
        a.options.len()
    }
}

/// Advance to the next option. Wraps around. When the server sends no
/// options, uses the built-in display order (Yes → Yes-don't-ask → No);
/// when it does, cycles linearly through the dynamic list.
pub(super) fn approval_next(current: usize, count: usize) -> usize {
    if count == crate::records::APPROVAL_OPTIONS {
        let pos = APPROVAL_DISPLAY_ORDER
            .iter()
            .position(|&s| s == current)
            .unwrap_or(0);
        APPROVAL_DISPLAY_ORDER[(pos + 1) % APPROVAL_DISPLAY_ORDER.len()]
    } else if count == 0 {
        0
    } else {
        (current + 1) % count
    }
}

/// Advance to the previous option. Wraps around.
pub(super) fn approval_prev(current: usize, count: usize) -> usize {
    if count == crate::records::APPROVAL_OPTIONS {
        let pos = APPROVAL_DISPLAY_ORDER
            .iter()
            .position(|&s| s == current)
            .unwrap_or(0);
        APPROVAL_DISPLAY_ORDER
            [(pos + APPROVAL_DISPLAY_ORDER.len() - 1) % APPROVAL_DISPLAY_ORDER.len()]
    } else if count == 0 {
        0
    } else {
        (current + count - 1) % count
    }
}
