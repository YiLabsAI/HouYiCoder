//! Three-state optional fields for protocol messages where a JSON null and
//! an omitted field carry distinct semantics.
//!
//! Some wire fields (a session title update, for instance) are three-state:
//! an absent field means leave the stored value unchanged, a present null
//! means clear the value, and a present value means set it. A plain Option
//! only has two states and collapses the absent-vs-null distinction, so the
//! clear-value intent is lost on decode. This type restores the third state.
//!
//! Serde contract. The three states map onto the wire as follows:
//!
//! Undefined — the field is absent from the JSON object. This state is
//! produced by the owning struct: the field carries serde(default) so an
//! absent key deserializes to the default, which is Undefined. On serialize,
//! the field carries skip_serializing_if = MaybeUndefined::is_undefined so
//! Undefined is never emitted. A direct serialize of an Undefined value
//! falls back to null (see below); the field-level skip is the real
//! mechanism, and a holder struct must use it for the absent state to round
//! trip.
//!
//! Null — the field is present as JSON null. Serializes to null;
//! deserializes from null. This is the clear-the-value intent.
//!
//! Value — the field is present with a value. Serializes to the value;
//! deserializes from the value.
//!
//! Why a custom Deserialize rather than a derived one. A derive cannot tell
//! absent-from-null apart: serde hands the field deserializer only the value
//! that is present, never a presence flag, so a derived impl has nowhere to
//! put the absent signal. The split below is the standard idiom for a
//! three-state field without adding a dependency: Deserialize treats
//! Option Some as Value and Option None as Null, while the owning struct
//! supplies Undefined through serde(default) on the field.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A three-state optional: omitted, null, or a value.
///
/// See the module docs for the serde contract. In short: Undefined is
/// produced by serde(default) on the field and skipped on serialize via
/// is_undefined; Null and Value serialize and deserialize as null and the
/// value respectively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaybeUndefined<T> {
    /// The field was absent on the wire. Produced by serde(default) on the
    /// owning field; never produced by Deserialize directly, since a value
    /// deserializer has no absent signal.
    Undefined,
    /// The field was present as JSON null. The clear-the-value intent.
    Null,
    /// The field was present with a value. The set-the-value intent.
    Value(T),
}

impl<T> Default for MaybeUndefined<T> {
    /// Defaults to Undefined so serde(default) on a field yields the
    /// absent state, not a null-emit.
    fn default() -> Self {
        MaybeUndefined::Undefined
    }
}

impl<T> MaybeUndefined<T> {
    /// True when the field is the absent state. Intended for
    /// skip_serializing_if on the owning field so Undefined is never
    /// emitted and the absent state round trips.
    pub fn is_undefined(&self) -> bool {
        matches!(self, MaybeUndefined::Undefined)
    }

    /// True when the field is the clear-the-value state.
    pub fn is_null(&self) -> bool {
        matches!(self, MaybeUndefined::Null)
    }

    /// True when the field is the set-the-value state.
    pub fn is_value(&self) -> bool {
        matches!(self, MaybeUndefined::Value(_))
    }

    /// The contained value when present, else None. Mirrors Option::as_ref
    /// so callers can borrow without consuming.
    pub fn as_value(&self) -> Option<&T> {
        match self {
            MaybeUndefined::Value(t) => Some(t),
            _ => None,
        }
    }

    /// The contained value when present, consuming the wrapper. Mirrors
    /// Option::unwrap_or as a non-panicking extraction.
    pub fn into_value(self) -> Option<T> {
        match self {
            MaybeUndefined::Value(t) => Some(t),
            _ => None,
        }
    }

    /// Apply a function to the contained value when present, preserving the
    /// Undefined and Null states. Lets a caller refine the value type
    /// without re-implementing the three-state plumbing.
    pub fn map_value<U, F: FnOnce(T) -> U>(self, f: F) -> MaybeUndefined<U> {
        match self {
            MaybeUndefined::Undefined => MaybeUndefined::Undefined,
            MaybeUndefined::Null => MaybeUndefined::Null,
            MaybeUndefined::Value(t) => MaybeUndefined::Value(f(t)),
        }
    }
}

impl<T: fmt::Display> fmt::Display for MaybeUndefined<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MaybeUndefined::Undefined => f.write_str("<undefined>"),
            MaybeUndefined::Null => f.write_str("null"),
            MaybeUndefined::Value(t) => write!(f, "{t}"),
        }
    }
}

/// Serialize: Null emits null, Value emits the value. Undefined also emits
/// null as a defensive fallback for the rare case where the value is
/// serialized outside a struct field that carries the skip attribute; the
/// absent state is meant to be skipped at the field level, so a holder must
/// use skip_serializing_if = MaybeUndefined::is_undefined for Undefined to
/// be omitted. When the skip is in place, this branch is unreachable for
/// Undefined.
impl<T> Serialize for MaybeUndefined<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            MaybeUndefined::Undefined => serializer.serialize_none(),
            MaybeUndefined::Null => serializer.serialize_none(),
            MaybeUndefined::Value(t) => serializer.serialize_some(t),
        }
    }
}

/// Deserialize: a present value becomes Value, a present null becomes Null.
/// Undefined is never produced here; it is supplied by serde(default) on the
/// owning field when the key is absent (serde does not call the field
/// deserializer at all for an absent key).
impl<'de, T> Deserialize<'de> for MaybeUndefined<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<T>::deserialize(deserializer)?;
        Ok(match opt {
            Some(t) => MaybeUndefined::Value(t),
            None => MaybeUndefined::Null,
        })
    }
}

#[cfg(test)]
mod tests;
