//! Tests for this module.

use super::MaybeUndefined;

/// A holder struct that mirrors the real usage: the field carries
/// serde(default) for the absent state and skip_serializing_if for the
/// omit-on-serialize contract. The wire shape is exactly the field
/// states the three-state contract claims.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Holder {
    #[serde(default, skip_serializing_if = "MaybeUndefined::is_undefined")]
    title: MaybeUndefined<String>,
}

#[test]
fn test_value_round_trips() {
    let h = Holder {
        title: MaybeUndefined::Value("hello".into()),
    };
    let json = serde_json::to_string(&h).expect("serialize");
    assert_eq!(json, r#"{"title":"hello"}"#);
    let back: Holder = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, h);
}

#[test]
fn test_null_round_trips() {
    let h = Holder {
        title: MaybeUndefined::Null,
    };
    let json = serde_json::to_string(&h).expect("serialize");
    assert_eq!(json, r#"{"title":null}"#);
    let back: Holder = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, h);
    // Null must not collapse to Undefined: the clear intent survives.
    assert!(back.title.is_null());
    assert!(!back.title.is_undefined());
}

#[test]
fn test_undefined_skipped_on_serialize() {
    let h = Holder {
        title: MaybeUndefined::Undefined,
    };
    let json = serde_json::to_string(&h).expect("serialize");
    // Undefined emits no key at all.
    assert_eq!(json, "{}");
}

#[test]
fn test_round_trips_via_default() {
    // An absent key deserializes to Undefined through serde(default),
    // which is the only path that produces the absent state.
    let json = "{}";
    let back: Holder = serde_json::from_str(json).expect("deserialize");
    assert_eq!(back.title, MaybeUndefined::Undefined);
    assert!(back.title.is_undefined());
}

#[test]
fn test_null_vs_round_trip() {
    // The whole point: null and absent must not collapse. A null field
    // round trips to Null; an absent field round trips to Undefined.
    let null_h: Holder = serde_json::from_str(r#"{"title":null}"#).expect("null");
    let absent_h: Holder = serde_json::from_str("{}").expect("absent");
    assert_eq!(null_h.title, MaybeUndefined::Null);
    assert_eq!(absent_h.title, MaybeUndefined::Undefined);
    assert_ne!(null_h, absent_h);
}

#[test]
fn test_helpers_report_state() {
    let u = MaybeUndefined::<String>::Undefined;
    assert!(u.is_undefined());
    assert!(!u.is_null());
    assert!(!u.is_value());
    assert_eq!(u.as_value(), None);
    assert_eq!(u.into_value(), None);

    let n = MaybeUndefined::<String>::Null;
    assert!(!n.is_undefined());
    assert!(n.is_null());
    assert!(!n.is_value());
    assert_eq!(n.as_value(), None);

    let v = MaybeUndefined::Value("x".to_string());
    assert!(!v.is_undefined());
    assert!(!v.is_null());
    assert!(v.is_value());
    assert_eq!(v.as_value(), Some(&"x".to_string()));
    assert_eq!(v.into_value(), Some("x".to_string()));
}

#[test]
fn test_map_value_preserves_state() {
    let u = MaybeUndefined::<String>::Undefined;
    assert_eq!(u.map_value(|s| s.len()), MaybeUndefined::Undefined);

    let n = MaybeUndefined::<String>::Null;
    assert_eq!(n.map_value(|s| s.len()), MaybeUndefined::Null);

    let v = MaybeUndefined::Value("four".to_string());
    assert_eq!(v.map_value(|s| s.len()), MaybeUndefined::Value(4));
}

#[test]
fn test_default_is_undefined() {
    assert_eq!(
        MaybeUndefined::<String>::default(),
        MaybeUndefined::Undefined
    );
}
