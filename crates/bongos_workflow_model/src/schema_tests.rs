use serde_json::json;
use tempfile::TempDir;

use crate::definition::{FieldType, ItemType, Node, Outcome, OutputField, parse};
use crate::schema::{
    CheckError, OutputSchema, TypedValue, check, document, format_errors, json_schema,
    json_schema_strict, validate,
};

/// The output fields of one node declared with `spec`.
fn output(spec: &str) -> Vec<OutputField> {
    let text = format!(
        "schema = 1\nname = \"s\"\nstart = \"a\"\n[nodes.a]\nkind = \"agent\"\noutput = {spec}\n"
    );
    parse(&text).unwrap().nodes[0].output.clone()
}

fn schema_of(fields: &[OutputField]) -> OutputSchema {
    OutputSchema::from_fields(fields).unwrap()
}

fn every_type() -> Vec<OutputField> {
    output(
        r#"{ verdict = { type = "choice", values = ["approve", "changes"] }, summary = { type = "text" }, design = { type = "path", outcome = "document" }, score = { type = "number", optional = true }, items = { type = "list", of = "text" }, files = { type = "list", of = "path", optional = true }, sizes = { type = "list", of = "number", optional = true } }"#,
    )
}

#[test]
fn json_schema_covers_every_field_type() {
    let schema = json_schema(&every_type());
    assert_eq!(
        schema,
        json!({
            "type": "object",
            "properties": {
                "verdict": { "type": "string", "enum": ["approve", "changes"] },
                "summary": { "type": "string" },
                "design": { "type": "string" },
                "score": { "type": "number" },
                "items": { "type": "array", "items": { "type": "string" } },
                "files": { "type": "array", "items": { "type": "string" } },
                "sizes": { "type": "array", "items": { "type": "number" } },
            },
            "required": ["verdict", "summary", "design", "items"],
            "additionalProperties": false,
        })
    );
    // Properties keep the declaration order.
    let keys = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "verdict", "summary", "design", "score", "items", "files", "sizes"
        ]
    );
}

#[test]
fn strict_schema_requires_every_property_and_nulls_the_optional_ones() {
    let schema = json_schema_strict(&json_schema(&every_type()));
    assert_eq!(
        schema["required"],
        json!([
            "verdict", "summary", "design", "score", "items", "files", "sizes"
        ])
    );
    assert_eq!(
        schema["properties"]["score"]["type"],
        json!(["number", "null"])
    );
    assert_eq!(
        schema["properties"]["files"]["type"],
        json!(["array", "null"])
    );
    assert_eq!(schema["properties"]["summary"]["type"], json!("string"));
    assert_eq!(schema["additionalProperties"], json!(false));
    // Null is tolerated for an optional field when the result is read.
    let result = check(
        &schema_of(&every_type()),
        &json!({
            "verdict": "approve", "summary": "s", "design": "d.md", "score": null,
            "items": [], "files": null, "sizes": null,
        }),
    )
    .unwrap();
    assert!(result.get("score").is_none());
}

#[test]
fn json_schema_of_no_fields() {
    assert_eq!(
        json_schema(&[]),
        json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false })
    );
}

#[test]
fn check_accepts_every_field_type() {
    let result = check(
        &schema_of(&every_type()),
        &json!({
            "verdict": "approve",
            "summary": "done",
            "design": "docs/design.md",
            "score": 4.5,
            "items": ["a", "b"],
            "files": ["src/a.rs", "./src/b.rs"],
            "sizes": [1, 2.5],
        }),
    )
    .unwrap();
    assert_eq!(
        result.get("verdict"),
        Some(&TypedValue::Choice("approve".into()))
    );
    assert_eq!(
        result.get("summary"),
        Some(&TypedValue::Text("done".into()))
    );
    assert_eq!(
        result.get("design"),
        Some(&TypedValue::Path("docs/design.md".into()))
    );
    assert_eq!(result.get("score"), Some(&TypedValue::Number(4.5)));
    assert_eq!(
        result.get("items"),
        Some(&TypedValue::List(vec![
            TypedValue::Text("a".into()),
            TypedValue::Text("b".into())
        ]))
    );
    assert_eq!(
        result.get("sizes"),
        Some(&TypedValue::List(vec![
            TypedValue::Number(1.0),
            TypedValue::Number(2.5)
        ]))
    );
    assert_eq!(
        result.get("verdict").and_then(TypedValue::as_str),
        Some("approve")
    );
    let outcomes = result.outcomes().collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        [(
            Outcome::Document,
            &TypedValue::Path("docs/design.md".into())
        )]
    );
    assert_eq!(result.to_json()["sizes"], json!([1.0, 2.5]));
    assert_eq!(result.to_json()["verdict"], json!("approve"));
}

#[test]
fn optional_fields_may_be_absent_or_null() {
    let result = check(
        &schema_of(&every_type()),
        &json!({
            "verdict": "changes",
            "summary": "",
            "design": "d.md",
            "score": null,
            "items": [],
        }),
    )
    .unwrap();
    assert_eq!(result.get("score"), None);
    assert_eq!(result.get("files"), None);
    assert_eq!(result.get("items"), Some(&TypedValue::List(Vec::new())));
    assert_eq!(result.fields.len(), 4);
}

#[test]
fn missing_fields_are_refused() {
    let error = check(
        &schema_of(&every_type()),
        &json!({ "verdict": "approve", "summary": "s", "items": [] }),
    )
    .unwrap_err();
    assert_eq!(
        error,
        "Result failed validation at `/design`: expected a value, got nothing"
    );
    // A required field may not be null.
    let error = check(
        &schema_of(&output(r#"{ summary = { type = "text" } }"#)),
        &json!({ "summary": null }),
    )
    .unwrap_err();
    assert_eq!(
        error,
        "Result failed validation at `/summary`: expected a string, got null"
    );
}

#[test]
fn extra_fields_are_refused() {
    let error = check(
        &schema_of(&output(r#"{ summary = { type = "text" } }"#)),
        &json!({ "summary": "s", "mood": "good" }),
    )
    .unwrap_err();
    assert_eq!(
        error,
        "Result failed validation at `/mood`: expected no such field, got \"good\""
    );
}

#[test]
fn choice_values_are_closed() {
    let fields = output(r#"{ verdict = { type = "choice", values = ["approve", "changes"] } }"#);
    let error = check(&schema_of(&fields), &json!({ "verdict": "maybe" })).unwrap_err();
    assert_eq!(
        error,
        "Result failed validation at `/verdict`: expected one of \"approve\", \"changes\", got \"maybe\""
    );
    let error = check(&schema_of(&fields), &json!({ "verdict": 1 })).unwrap_err();
    assert_eq!(
        error,
        "Result failed validation at `/verdict`: expected a string, got 1"
    );
}

#[test]
fn wrong_types_are_refused() {
    let cases = [
        (
            r#"{ f = { type = "text" } }"#,
            json!({ "f": 3 }),
            "Result failed validation at `/f`: expected a string, got 3",
        ),
        (
            r#"{ f = { type = "path" } }"#,
            json!({ "f": ["a"] }),
            "Result failed validation at `/f`: expected a string, got [\"a\"]",
        ),
        (
            r#"{ f = { type = "number" } }"#,
            json!({ "f": "3" }),
            "Result failed validation at `/f`: expected a number, got \"3\"",
        ),
        (
            r#"{ f = { type = "list", of = "text" } }"#,
            json!({ "f": "a" }),
            "Result failed validation at `/f`: expected a list, got \"a\"",
        ),
        (
            r#"{ f = { type = "list", of = "number" } }"#,
            json!({ "f": [1, "2"] }),
            "Result failed validation at `/f/1`: expected a number, got \"2\"",
        ),
        (
            r#"{ f = { type = "list", of = "path" } }"#,
            json!({ "f": ["ok.md", true] }),
            "Result failed validation at `/f/1`: expected a string, got true",
        ),
    ];
    for (spec, value, expected) in cases {
        assert_eq!(
            check(&schema_of(&output(spec)), &value).unwrap_err(),
            expected,
            "{spec}"
        );
    }
}

#[test]
fn paths_stay_inside_the_worktree() {
    let fields = output(r#"{ f = { type = "path" } }"#);
    for path in ["/etc/passwd", "../outside.md", "a/../../b", "", ".", "a/.."] {
        let error = check(&schema_of(&fields), &json!({ "f": path })).unwrap_err();
        assert!(
            error.contains("expected a relative path inside the worktree"),
            "{path}: {error}"
        );
    }
    for path in ["a.md", "./a.md", "a/../b.md", "docs/sub/c.md"] {
        assert!(
            check(&schema_of(&fields), &json!({ "f": path })).is_ok(),
            "{path}"
        );
    }
}

#[test]
fn results_must_be_objects() {
    let fields = output(r#"{ f = { type = "text" } }"#);
    assert_eq!(
        check(&schema_of(&fields), &json!(["f"])).unwrap_err(),
        "Result failed validation at `/`: expected an object, got [\"f\"]"
    );
    assert_eq!(
        check(&schema_of(&fields), &json!(null)).unwrap_err(),
        "Result failed validation at `/`: expected an object, got null"
    );
}

#[test]
fn every_problem_is_listed() {
    let error = check(
        &schema_of(&output(
            r#"{ a = { type = "text" }, b = { type = "number" } }"#,
        )),
        &json!({ "b": "x", "c": 1 }),
    )
    .unwrap_err();
    assert_eq!(
        error,
        "Result failed validation at `/a`: expected a value, got nothing\nResult failed validation at `/b`: expected a number, got \"x\"\nResult failed validation at `/c`: expected no such field, got 1"
    );
}

fn findings_schema() -> OutputSchema {
    let document = json!({
        "type": "object",
        "properties": {
            "verdict": { "type": "string", "enum": ["approve", "changes"] },
            "note": { "type": "string" },
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "score": { "type": "number" } },
                    "required": ["score"],
                },
            },
        },
        "required": ["verdict", "findings"],
        "additionalProperties": false,
    });
    OutputSchema::compile(document, Vec::new()).unwrap()
}

fn errors_of(schema: &OutputSchema, value: serde_json::Value) -> Vec<CheckError> {
    validate(schema, &value).unwrap_err()
}

#[test]
fn validate_reports_a_wrong_type_at_a_nested_pointer() {
    let errors = errors_of(
        &findings_schema(),
        json!({ "verdict": "approve", "findings": [{ "score": 1 }, { "score": 2 }, { "score": "high" }] }),
    );
    assert_eq!(
        errors,
        vec![CheckError {
            path: "/findings/2/score".to_string(),
            value: Some(json!("high")),
            expected: "a number".to_string(),
        }]
    );
}

#[test]
fn validate_reports_a_missing_field_at_its_own_pointer() {
    let errors = errors_of(
        &findings_schema(),
        json!({ "verdict": "approve", "findings": [{}] }),
    );
    assert_eq!(
        errors,
        vec![CheckError {
            path: "/findings/0/score".to_string(),
            value: None,
            expected: "a value".to_string(),
        }]
    );
    assert_eq!(
        format_errors(&errors),
        "Result failed validation at `/findings/0/score`: expected a value, got nothing"
    );
}

#[test]
fn validate_reports_an_extra_field() {
    let errors = errors_of(
        &findings_schema(),
        json!({ "verdict": "approve", "findings": [], "extra": 7 }),
    );
    assert_eq!(
        format_errors(&errors),
        "Result failed validation at `/extra`: expected no such field, got 7"
    );
}

#[test]
fn validate_reports_an_enum_miss() {
    let errors = errors_of(
        &findings_schema(),
        json!({ "verdict": "maybe", "findings": [] }),
    );
    assert_eq!(
        format_errors(&errors),
        "Result failed validation at `/verdict`: expected one of \"approve\", \"changes\", got \"maybe\""
    );
}

#[test]
fn validate_reports_a_root_that_is_not_an_object() {
    let errors = errors_of(&findings_schema(), json!(["x"]));
    assert_eq!(
        errors,
        vec![CheckError {
            path: "/".to_string(),
            value: Some(json!(["x"])),
            expected: "an object".to_string(),
        }]
    );
}

#[test]
fn format_errors_lists_ten_then_counts_the_rest() {
    let findings = (0..11).map(|_| json!({ "score": "x" })).collect::<Vec<_>>();
    let errors = errors_of(
        &findings_schema(),
        json!({ "verdict": "approve", "findings": findings }),
    );
    assert_eq!(errors.len(), 11);
    let message = format_errors(&errors);
    let lines = message.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 11);
    assert_eq!(
        lines[0],
        "Result failed validation at `/findings/0/score`: expected a number, got \"x\""
    );
    assert_eq!(lines[10], "and 1 more");
}

#[test]
fn validate_sorts_errors_by_path() {
    let errors = errors_of(
        &findings_schema(),
        json!({ "zzz": 1, "findings": [{ "score": "a" }] }),
    );
    let paths = errors.iter().map(|e| e.path.as_str()).collect::<Vec<_>>();
    assert_eq!(paths, ["/findings/0/score", "/verdict", "/zzz"]);
}

#[test]
fn validate_accepts_null_for_an_optional_field() {
    let fields = output(r#"{ a = { type = "text" }, b = { type = "number", optional = true } }"#);
    let strict =
        OutputSchema::compile(json_schema_strict(&json_schema(&fields)), fields.clone()).unwrap();
    assert_eq!(validate(&strict, &json!({ "a": "x", "b": null })), Ok(()));
    assert_eq!(validate(&strict, &json!({ "a": "x", "b": 2 })), Ok(()));
    // A required field set to null is still an error.
    assert!(validate(&strict, &json!({ "a": null, "b": null })).is_err());
    // Non-strict schema: a property outside `required` may be null too.
    let loose = OutputSchema::compile(json_schema(&fields), Vec::new()).unwrap();
    assert_eq!(validate(&loose, &json!({ "a": "x", "b": null })), Ok(()));
}

/// A node naming `schemas/result.json` with `schema` written to it in a fresh directory.
fn file_node(schema: &str) -> (TempDir, Node) {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join("schemas")).unwrap();
    std::fs::write(dir.path().join("schemas/result.json"), schema).unwrap();
    let text = "schema = 1\nname = \"s\"\nstart = \"a\"\n[nodes.a]\nkind = \"agent\"\noutput_schema = \"schemas/result.json\"\n";
    (dir, parse(text).unwrap().nodes[0].clone())
}

fn loaded(schema: &str) -> OutputSchema {
    let (dir, node) = file_node(schema);
    document(&node, dir.path()).unwrap()
}

#[test]
fn a_disallowed_schema_keyword_is_a_load_error_naming_the_keyword_and_its_path() {
    for (keyword, schema, path) in [
        (
            "$ref",
            r##"{"type":"object","properties":{"a":{"$ref":"#/$defs/x"}}}"##,
            "/properties/a/$ref",
        ),
        (
            "pattern",
            r#"{"type":"object","properties":{"a":{"type":"string","pattern":"^x"}}}"#,
            "/properties/a/pattern",
        ),
        (
            "oneOf",
            r#"{"type":"object","oneOf":[{"type":"object"}]}"#,
            "/oneOf",
        ),
    ] {
        let (dir, node) = file_node(schema);
        let message = document(&node, dir.path()).unwrap_err().message;
        assert!(
            message.contains(&format!("keyword '{keyword}' is not allowed at {path}")),
            "{message}"
        );
    }
}

#[test]
fn a_file_schema_maps_to_the_five_field_types() {
    let schema = loaded(
        r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","additionalProperties":false,
        "required":["verdict","summary","score","count","tags","sizes"],
        "properties":{
          "verdict":{"type":"string","enum":["ship","hold"]},
          "summary":{"type":"string","title":"s","description":"d"},
          "score":{"type":"number"},
          "count":{"type":"integer"},
          "tags":{"type":"array","items":{"type":"string"}},
          "sizes":{"type":"array","items":{"type":"number"}},
          "note":{"type":"string"}}}"#,
    );
    let types = schema
        .fields()
        .iter()
        .map(|field| {
            (
                field.name.as_str(),
                field.field_type.clone(),
                field.optional,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        types,
        [
            (
                "verdict",
                FieldType::Choice(vec!["ship".into(), "hold".into()]),
                false
            ),
            ("summary", FieldType::Text, false),
            ("score", FieldType::Number, false),
            ("count", FieldType::Number, false),
            ("tags", FieldType::List(ItemType::Text), false),
            ("sizes", FieldType::List(ItemType::Number), false),
            ("note", FieldType::Text, true),
        ]
    );
}

#[test]
fn a_nested_object_in_a_schema_becomes_a_data_field() {
    let schema = loaded(
        r#"{"type":"object","required":["report"],"properties":{"report":{"type":"object","properties":{"n":{"type":"number"}}}}}"#,
    );
    assert_eq!(schema.fields()[0].field_type, FieldType::Data);
    let typed = check(&schema, &json!({"report": {"n": 1}})).unwrap();
    assert_eq!(
        typed.get("report"),
        Some(&TypedValue::Data(json!({"n": 1})))
    );
    assert_eq!(typed.to_json(), json!({"report": {"n": 1}}));
    assert_eq!(typed.outcomes().count(), 0);
}

#[test]
fn a_missing_schema_file_is_a_load_error() {
    let (_dir, node) = file_node("{}");
    let empty = TempDir::new().unwrap();
    let diagnostic = document(&node, empty.path()).unwrap_err();
    assert!(diagnostic.is_error());
    assert!(
        diagnostic
            .message
            .contains("cannot read output_schema 'schemas/result.json'"),
        "{}",
        diagnostic.message
    );
    assert_eq!(diagnostic.node.as_deref(), Some("a"));
}

#[test]
fn naming_both_output_and_output_schema_is_a_load_error() {
    let text = "schema = 1\nname = \"s\"\nstart = \"a\"\n[nodes.a]\nkind = \"agent\"\noutput = { x = { type = \"text\" } }\noutput_schema = \"schemas/result.json\"\n";
    let node = parse(text).unwrap().nodes[0].clone();
    let diagnostic = document(&node, TempDir::new().unwrap().path()).unwrap_err();
    assert!(
        diagnostic.message.contains("not both"),
        "{}",
        diagnostic.message
    );
}

#[test]
fn the_generated_document_and_the_strict_schema_derived_from_it_are_unchanged() {
    let fields = every_type();
    let generated = json_schema(&fields);
    let pinned = r#"{"additionalProperties":false,"properties":{"design":{"type":"string"},"files":{"items":{"type":"string"},"type":"array"},"items":{"items":{"type":"string"},"type":"array"},"score":{"type":"number"},"sizes":{"items":{"type":"number"},"type":"array"},"summary":{"type":"string"},"verdict":{"enum":["approve","changes"],"type":"string"}},"required":["verdict","summary","design","items"],"type":"object"}"#;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(pinned).unwrap(),
        generated
    );
    // The node's own document is that generated one.
    let node = parse(
        "schema = 1\nname = \"s\"\nstart = \"a\"\n[nodes.a]\nkind = \"agent\"\noutput = { v = { type = \"choice\", values = [\"x\"], optional = true }, t = { type = \"text\" } }\n",
    )
    .unwrap()
    .nodes[0]
        .clone();
    assert_eq!(node.schema.document(), &json_schema(&node.output));
    let strict = json_schema_strict(&node.schema.document().clone());
    assert_eq!(
        strict,
        json!({
            "type": "object",
            "properties": {
                "v": { "type": ["string", "null"], "enum": ["x", null] },
                "t": { "type": "string" },
            },
            "required": ["v", "t"],
            "additionalProperties": false,
        })
    );
}

fn graph_edits_schema_of_field() -> OutputSchema {
    schema_of(&output("{ edits = { type = \"graph_edits\" } }"))
}

#[test]
fn graph_edits_field_accepts_every_operation() {
    let schema = graph_edits_schema_of_field();
    let ops = json!([
        { "op": "create", "ref": "a", "parent": "t1", "goal": "do it", "work_kind": "step",
          "areas": ["search"], "size": 2, "repositories": ["app"],
          "todos": [{ "name": "first", "workflow": "implement" }] },
        { "op": "set", "task": "@a", "goal": "renamed", "work_kind": "step", "size": 3,
          "areas": ["ui"] },
        { "op": "move", "task": "t2", "parent": "@a" },
        { "op": "remove", "task": "t3" },
        { "op": "add_edge", "from": "@a", "to": "t4", "kind": "after", "unblock": "merged" },
        { "op": "add_edge", "from": "t4", "to": "t5", "kind": "stacked_on" },
        { "op": "remove_edge", "from": "@a", "to": "t4", "kind": "after" },
    ]);
    check(
        &schema,
        &json!({ "edits": { "project": "website", "ops": ops } }),
    )
    .unwrap();
    check(&schema, &json!({ "edits": { "ops": [] } })).unwrap();
}

#[test]
fn graph_edits_field_rejects_unknown_operations() {
    let schema = graph_edits_schema_of_field();
    for bad in [
        json!({ "op": "teleport", "task": "t1" }),
        json!({ "op": "remove" }),
        json!({ "op": "remove", "task": "t1", "force": true }),
        json!({ "op": "add_edge", "from": "a", "to": "b", "kind": "before" }),
        json!({ "op": "create", "parent": "t1" }),
    ] {
        assert!(
            check(&schema, &json!({ "edits": { "ops": [bad.clone()] } })).is_err(),
            "{bad} must be refused"
        );
    }
    assert!(check(&schema, &json!({ "edits": { "project": "w" } })).is_err());
    assert!(check(&schema, &json!({ "edits": "none" })).is_err());
}
