//! The AskUserQuestion card: parsing and answer building, plus the render and
//! height rules its layout follows. Crate-visible because a sibling test
//! builds a card through these fixtures rather than restating them.

#[cfg(test)]
pub(crate) mod card;
#[cfg(test)]
mod render;
