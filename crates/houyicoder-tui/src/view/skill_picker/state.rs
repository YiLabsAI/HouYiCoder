//! The inline skill-picker popup state: the open flag and its list cursor.
//! One owner because opening the picker writes the open flag and resets the
//! cursor to zero as one value, rather than two flat fields a caller must
//! set in step.

use std::cell::Cell;

#[derive(Debug, Default)]
pub(crate) struct SkillPickerState {
    pub(crate) open: bool,
    pub(crate) sel: Cell<usize>,
}
