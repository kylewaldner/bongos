use serde_json::json;

use crate::definition::{Outcome, OutputField, parse};
use crate::schema::{TypedValue, check, json_schema, json_schema_strict};

/// The output fields of one node declared with `spec`.
fn output(spec: &str) -> Vec<OutputField> {
    let text = format!(
        "schema = 1\nname = \"s\"\nstart = \"a\"\n[nodes.a]\nkind = \"agent\"\noutput = {spec}\n"
    );
    parse(&text).unwrap().nodes[0].output.clone()
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
    let schema = json_schema_strict(&every_type());
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
        &every_type(),
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
        &every_type(),
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
        &every_type(),
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
        &every_type(),
        &json!({ "verdict": "approve", "summary": "s", "items": [] }),
    )
    .unwrap_err();
    assert_eq!(error, "missing field 'design'");
    // A required field may not be null.
    let error = check(
        &output(r#"{ summary = { type = "text" } }"#),
        &json!({ "summary": null }),
    )
    .unwrap_err();
    assert_eq!(error, "field 'summary': expected a string, got null");
}

#[test]
fn extra_fields_are_refused() {
    let error = check(
        &output(r#"{ summary = { type = "text" } }"#),
        &json!({ "summary": "s", "mood": "good" }),
    )
    .unwrap_err();
    assert_eq!(error, "unexpected field 'mood'");
}

#[test]
fn choice_values_are_closed() {
    let fields = output(r#"{ verdict = { type = "choice", values = ["approve", "changes"] } }"#);
    let error = check(&fields, &json!({ "verdict": "maybe" })).unwrap_err();
    assert_eq!(
        error,
        "field 'verdict': 'maybe' is not one of 'approve', 'changes'"
    );
    let error = check(&fields, &json!({ "verdict": 1 })).unwrap_err();
    assert_eq!(error, "field 'verdict': expected a string, got a number");
}

#[test]
fn wrong_types_are_refused() {
    let cases = [
        (
            r#"{ f = { type = "text" } }"#,
            json!({ "f": 3 }),
            "field 'f': expected a string, got a number",
        ),
        (
            r#"{ f = { type = "path" } }"#,
            json!({ "f": ["a"] }),
            "field 'f': expected a string, got a list",
        ),
        (
            r#"{ f = { type = "number" } }"#,
            json!({ "f": "3" }),
            "field 'f': expected a number, got a string",
        ),
        (
            r#"{ f = { type = "list", of = "text" } }"#,
            json!({ "f": "a" }),
            "field 'f': expected a list, got a string",
        ),
        (
            r#"{ f = { type = "list", of = "number" } }"#,
            json!({ "f": [1, "2"] }),
            "field 'f': item 1: expected a number, got a string",
        ),
        (
            r#"{ f = { type = "list", of = "path" } }"#,
            json!({ "f": ["ok.md", true] }),
            "field 'f': item 1: expected a string, got a boolean",
        ),
    ];
    for (spec, value, expected) in cases {
        assert_eq!(
            check(&output(spec), &value).unwrap_err(),
            expected,
            "{spec}"
        );
    }
}

#[test]
fn paths_stay_inside_the_worktree() {
    let fields = output(r#"{ f = { type = "path" } }"#);
    for path in ["/etc/passwd", "../outside.md", "a/../../b", "", ".", "a/.."] {
        let error = check(&fields, &json!({ "f": path })).unwrap_err();
        assert!(
            error.contains("is not a relative path inside the worktree"),
            "{path}: {error}"
        );
    }
    for path in ["a.md", "./a.md", "a/../b.md", "docs/sub/c.md"] {
        assert!(check(&fields, &json!({ "f": path })).is_ok(), "{path}");
    }
}

#[test]
fn results_must_be_objects() {
    let fields = output(r#"{ f = { type = "text" } }"#);
    assert_eq!(
        check(&fields, &json!(["f"])).unwrap_err(),
        "the result must be a JSON object, not a list"
    );
    assert_eq!(
        check(&fields, &json!(null)).unwrap_err(),
        "the result must be a JSON object, not null"
    );
}

#[test]
fn every_problem_is_listed() {
    let error = check(
        &output(r#"{ a = { type = "text" }, b = { type = "number" } }"#),
        &json!({ "b": "x", "c": 1 }),
    )
    .unwrap_err();
    assert_eq!(
        error,
        "missing field 'a'; field 'b': expected a number, got a string; unexpected field 'c'"
    );
}
