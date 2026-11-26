use std::collections::BTreeMap;

use bongos_work_model::UnblockPoint;
use bongos_workflow_model::definition::{FieldType, OutputField};
use bongos_workflow_model::json_schema;
use serde_json::{Value, json};

use crate::ops::{EdgeKind, GraphOp, TaskRef, validate_batch};

fn schema_ops() -> Vec<Value> {
    let schema = json_schema(&[OutputField {
        name: "edits".to_string(),
        line: 1,
        field_type: FieldType::GraphEdits,
        optional: false,
        outcome: None,
    }]);
    schema["properties"]["edits"]["properties"]["ops"]["items"]["oneOf"]
        .as_array()
        .expect("the ops are a oneOf")
        .clone()
}

#[test]
fn schema_op_list_equals_the_ops_enum() {
    let names: Vec<String> = schema_ops()
        .iter()
        .map(|op| {
            op["properties"]["op"]["const"]
                .as_str()
                .expect("each op names itself")
                .to_string()
        })
        .collect();
    assert_eq!(names, GraphOp::NAMES.map(str::to_string).to_vec());
    // Each op of the schema, with only its required fields, reads as the variant of its name.
    for op in schema_ops() {
        let name = op["properties"]["op"]["const"].as_str().unwrap();
        let mut value = serde_json::Map::new();
        for field in op["required"].as_array().unwrap() {
            let field = field.as_str().unwrap();
            let sample = match field {
                "op" => json!(name),
                "kind" => json!("after"),
                _ => json!("t1"),
            };
            value.insert(field.to_string(), sample);
        }
        let parsed: GraphOp = serde_json::from_value(Value::Object(value))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(parsed.name(), name);
    }
}

#[test]
fn every_schema_property_is_a_field_of_its_op() {
    for op in schema_ops() {
        let name = op["properties"]["op"]["const"].as_str().unwrap();
        let mut value = serde_json::Map::new();
        for (field, schema) in op["properties"].as_object().unwrap() {
            let sample = match (field.as_str(), schema["type"].as_str()) {
                ("op", _) => json!(name),
                ("kind", _) => json!("stacked_on"),
                ("unblock", _) => json!("review"),
                ("todos", _) => json!([{ "name": "plan", "workflow": "design" }]),
                (_, Some("array")) => json!(["one"]),
                (_, Some("integer")) => json!(2),
                _ => json!("t1"),
            };
            value.insert(field.clone(), sample);
        }
        let parsed: Result<GraphOp, _> = serde_json::from_value(Value::Object(value));
        assert!(parsed.is_ok(), "{name}: {parsed:?}");
    }
}

#[test]
fn the_design_examples_read() {
    let batch = json!({ "project": "p", "ops": [
        {"op":"create","ref":"a","parent":"t1","goal":"Index","work_kind":"step","areas":["search"],"size":2,"repositories":["app"],"todos":[{"name":"implement","workflow":"ship"}]},
        {"op":"set","task":"@a","goal":"Index files","work_kind":"step","size":3,"areas":["search"]},
        {"op":"move","task":"t2","parent":"@a"},
        {"op":"remove","task":"t3"},
        {"op":"add_edge","from":"@a","to":"t4","kind":"after","unblock":"implemented"},
        {"op":"remove_edge","from":"t5","to":"t4","kind":"stacked_on"},
    ]});
    let batch = validate_batch(&batch, &BTreeMap::new()).expect("valid");
    assert_eq!(batch.project.as_deref(), Some("p"));
    assert_eq!(batch.ops.len(), 6);
    assert_eq!(
        batch.ops[4],
        GraphOp::AddEdge {
            from: TaskRef::Ref("a".to_string()),
            to: TaskRef::Id("t4".to_string()),
            kind: EdgeKind::After,
            unblock: Some(UnblockPoint::Implemented),
        }
    );
    let round_trip: Value = serde_json::to_value(&batch).unwrap();
    assert_eq!(
        validate_batch(&round_trip, &BTreeMap::new()).unwrap(),
        batch
    );
}

fn error(batch: Value, known: &[(&str, &str)]) -> String {
    let known: BTreeMap<String, String> = known
        .iter()
        .map(|(name, id)| (name.to_string(), id.to_string()))
        .collect();
    validate_batch(&batch, &known)
        .expect_err("the batch is refused")
        .to_string()
}

#[test]
fn validate_batch_refuses_unknown_ops() {
    assert_eq!(
        error(
            json!({ "ops": [{"op":"remove","task":"t"}, {"op":"rename","task":"t"}] }),
            &[]
        ),
        "op 2: unknown op `rename`; use one of create, set, move, remove, add_edge, remove_edge"
    );
}

#[test]
fn validate_batch_refuses_duplicate_refs() {
    let create = json!({"op":"create","ref":"a","parent":"t","goal":"One"});
    assert_eq!(
        error(json!({ "ops": [create.clone(), create] }), &[]),
        "op 2: duplicate ref `a`"
    );
}

#[test]
fn validate_batch_refuses_unknown_refs() {
    assert_eq!(
        error(json!({ "ops": [{"op":"remove","task":"@ghost"}] }), &[]),
        "op 1: unknown ref `@ghost`"
    );
    // A ref used before its create is unknown there.
    assert_eq!(
        error(
            json!({ "ops": [
                {"op":"remove","task":"@a"},
                {"op":"create","ref":"a","parent":"t","goal":"One"},
            ] }),
            &[]
        ),
        "op 1: unknown ref `@a`"
    );
}

#[test]
fn validate_batch_refuses_an_empty_goal_on_create() {
    assert_eq!(
        error(
            json!({ "ops": [{"op":"create","parent":"t","goal":"  "}] }),
            &[]
        ),
        "op 1: create needs a goal"
    );
}

#[test]
fn validate_batch_accepts_refs_of_earlier_batches_and_rebuilds() {
    let known = BTreeMap::from([("a".to_string(), "id-a".to_string())]);
    let batch = json!({ "ops": [
        {"op":"remove","task":"@a"},
        {"op":"create","ref":"a","parent":"t","goal":"Again"},
    ] });
    assert!(validate_batch(&batch, &known).is_ok());
}

#[test]
fn validate_batch_refuses_unknown_fields() {
    let message = error(
        json!({ "ops": [{"op":"remove","task":"t","why":"old"}] }),
        &[],
    );
    assert!(
        message.starts_with("op 1: unknown field `why`"),
        "{message}"
    );
}

#[test]
fn task_refs_read_ids_and_refs() {
    let refs: Vec<TaskRef> = serde_json::from_value(json!(["t1", "@a"])).unwrap();
    assert_eq!(
        refs,
        vec![TaskRef::Id("t1".to_string()), TaskRef::Ref("a".to_string())]
    );
    assert_eq!(serde_json::to_value(&refs).unwrap(), json!(["t1", "@a"]));
}
