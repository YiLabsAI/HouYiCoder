//! The default required-argument check, which decides whether a model call can
//! run at all. The schema shapes covered here are the ones a tool may declare:
//! no required list, an empty one, a declared type, and a required name with
//! no declared type.

use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde_json::{Value, json};

use super::{Tool, ToolCtx};

/// A tool that declares whatever schema a test hands it and runs nothing.
struct SchemaTool(Value);

impl Tool for SchemaTool {
    fn name(&self) -> &str {
        "schema"
    }
    fn description(&self) -> &str {
        "declares the schema the test gives it"
    }
    fn input_schema(&self) -> Value {
        self.0.clone()
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(Value::Null) })
    }
}

fn invalid(schema: Value, input: Value) -> Vec<String> {
    SchemaTool(schema).invalid_required_arguments(&input)
}

/// A tool that accepts one declared field as empty.
struct EmptyTool(Value);

impl Tool for EmptyTool {
    fn name(&self) -> &str {
        "empty"
    }
    fn description(&self) -> &str {
        "accepts one field empty"
    }
    fn input_schema(&self) -> Value {
        self.0.clone()
    }
    fn empty_usable_arguments(&self) -> &'static [&'static str] {
        &["note"]
    }
    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async { Ok(Value::Null) })
    }
}

fn string_schema() -> Value {
    json!({
        "type": "object",
        "required": ["cmd"],
        "properties": {"cmd": {"type": "string"}},
    })
}

/// A required name is reported with the type the schema declares, so the model
/// reads what to supply rather than only which name it left out.
#[test]
fn test_missing_required_named() {
    assert_eq!(invalid(string_schema(), json!({})), ["cmd (string)"]);
}

/// An empty string, a null, and a value of the wrong JSON type are each as
/// unusable as an absent one: none of them can be run.
#[test]
fn test_unusable_values_rejected() {
    for input in [json!({"cmd": ""}), json!({"cmd": null}), json!({"cmd": 3})] {
        assert_eq!(
            invalid(string_schema(), input.clone()),
            ["cmd (string)"],
            "{input}"
        );
    }
}

/// Each declared type accepts its own values, including the falsy ones a naive
/// truthiness check would refuse: zero, false, and an empty container.
#[test]
fn test_declared_types_accepted() {
    let cases = [
        ("string", json!("ls")),
        ("integer", json!(0)),
        ("number", json!(0.5)),
        ("boolean", json!(false)),
        ("object", json!({})),
        ("array", json!([])),
    ];
    for (kind, value) in cases {
        let schema = json!({"required": ["x"], "properties": {"x": {"type": kind}}});
        assert_eq!(
            invalid(schema, json!({"x": value})),
            Vec::<String>::new(),
            "{kind}"
        );
    }
}

/// A tool that names a field as one it accepts empty: clearing that field is a
/// real call, while every other required string still needs a value.
#[test]
fn test_empty_usable_field_accepted() {
    let schema = json!({
        "required": ["note", "cmd"],
        "properties": {"note": {"type": "string"}, "cmd": {"type": "string"}},
    });
    let tool = EmptyTool(schema);
    assert!(
        tool.invalid_required_arguments(&json!({"note": "", "cmd": "ls"}))
            .is_empty()
    );
    assert_eq!(
        tool.invalid_required_arguments(&json!({"note": "", "cmd": ""})),
        ["cmd (string)"]
    );
}

/// A tool that declares no required arguments, or an empty list, accepts any
/// input at all, including one that is not an object.
#[test]
fn test_accepts_without_required() {
    for schema in [json!({"type": "object"}), json!({"required": []})] {
        assert_eq!(
            invalid(schema.clone(), json!({})),
            Vec::<String>::new(),
            "{schema}"
        );
        assert_eq!(
            invalid(schema.clone(), json!("raw")),
            Vec::<String>::new(),
            "{schema}"
        );
    }
}

/// An input that is not an object supplies none of the required names, so every
/// one of them is reported rather than the check passing on a shape it cannot
/// inspect.
#[test]
fn test_non_object_reported() {
    let schema = json!({
        "required": ["cmd", "cwd"],
        "properties": {"cmd": {"type": "string"}, "cwd": {"type": "string"}},
    });
    assert_eq!(
        invalid(schema, json!("raw")),
        ["cmd (string)", "cwd (string)"]
    );
}

/// A required name the schema gives no type for is usable when it holds
/// anything but null, and is reported by name alone.
#[test]
fn test_undeclared_type_rejects() {
    let schema = json!({"required": ["x"]});
    assert_eq!(
        invalid(schema.clone(), json!({"x": 0})),
        Vec::<String>::new()
    );
    assert_eq!(invalid(schema, json!({"x": null})), ["x"]);
}

/// A required entry that is not a string names no argument, so it is skipped
/// rather than reported as a name the model could never supply.
#[test]
fn test_required_entry_skipped() {
    let schema = json!({"required": ["cmd", 5]});
    assert_eq!(invalid(schema, json!({})), ["cmd"]);
}
