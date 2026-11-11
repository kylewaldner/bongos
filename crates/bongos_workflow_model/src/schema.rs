//! Typed node output: the JSON schema handed to an agent CLI, and the hand-written check of a result
//! against the declared fields (D4). Only the five field types exist, so no validator crate is used.

use serde_json::{Map, Value, json};

use crate::definition::{FieldType, ItemType, Outcome, OutputField};

/// A JSON schema for a node's output: an object with one property per field, `required` listing
/// every field not marked `optional`, and no other properties.
pub fn json_schema(output: &[OutputField]) -> Value {
    let properties = output
        .iter()
        .map(|field| (field.name.clone(), field_schema(&field.field_type)))
        .collect::<Map<_, _>>();
    let required = output
        .iter()
        .filter(|field| !field.optional)
        .map(|field| Value::String(field.name.clone()))
        .collect::<Vec<_>>();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn field_schema(field_type: &FieldType) -> Value {
    match field_type {
        FieldType::Choice(values) => json!({ "type": "string", "enum": values }),
        FieldType::Text | FieldType::Path => json!({ "type": "string" }),
        FieldType::Number => json!({ "type": "number" }),
        FieldType::List(item) => json!({ "type": "array", "items": item_schema(*item) }),
    }
}

fn item_schema(item: ItemType) -> Value {
    match item {
        ItemType::Text | ItemType::Path => json!({ "type": "string" }),
        ItemType::Number => json!({ "type": "number" }),
    }
}

/// One checked value of a result.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedValue {
    Choice(String),
    Text(String),
    /// Relative to the node's worktree, never absolute and never climbing out with `..`.
    Path(String),
    Number(f64),
    List(Vec<TypedValue>),
}

impl TypedValue {
    /// The value as JSON, as it is written to `result.json`.
    pub fn to_json(&self) -> Value {
        match self {
            TypedValue::Choice(value) | TypedValue::Text(value) | TypedValue::Path(value) => {
                Value::String(value.clone())
            }
            TypedValue::Number(value) => json!(value),
            TypedValue::List(items) => Value::Array(items.iter().map(Self::to_json).collect()),
        }
    }

    /// The string of a choice, text or path value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            TypedValue::Choice(value) | TypedValue::Text(value) | TypedValue::Path(value) => {
                Some(value)
            }
            TypedValue::Number(_) | TypedValue::List(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypedField {
    pub name: String,
    pub value: TypedValue,
    /// Set when the field is declared with `outcome` (D16).
    pub outcome: Option<Outcome>,
}

/// A result that matched its node's declared output. Fields keep the declaration order; an
/// optional field that was absent (or `null`) is not listed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TypedResult {
    pub fields: Vec<TypedField>,
}

impl TypedResult {
    pub fn get(&self, name: &str) -> Option<&TypedValue> {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| &field.value)
    }

    /// The fields marked as outcomes, with their values.
    pub fn outcomes(&self) -> impl Iterator<Item = (Outcome, &TypedValue)> {
        self.fields
            .iter()
            .filter_map(|field| field.outcome.map(|outcome| (outcome, &field.value)))
    }

    /// The result as a JSON object, as it is written to `result.json`.
    pub fn to_json(&self) -> Value {
        Value::Object(
            self.fields
                .iter()
                .map(|field| (field.name.clone(), field.value.to_json()))
                .collect(),
        )
    }
}

/// Checks a result against the declared fields: an object with every required field, no field
/// that is not declared, and every value of its declared type. An optional field may be absent or
/// `null`. Every problem found is listed in the error, separated by "; ".
pub fn check(output: &[OutputField], value: &Value) -> Result<TypedResult, String> {
    let Value::Object(object) = value else {
        return Err(format!(
            "the result must be a JSON object, not {}",
            kind_of(value)
        ));
    };

    let mut problems = Vec::new();
    let mut fields = Vec::new();
    for field in output {
        match object.get(&field.name) {
            None | Some(Value::Null) if field.optional => {}
            None => problems.push(format!("missing field '{}'", field.name)),
            Some(value) => match typed(&field.field_type, value) {
                Ok(typed) => fields.push(TypedField {
                    name: field.name.clone(),
                    value: typed,
                    outcome: field.outcome,
                }),
                Err(problem) => problems.push(format!("field '{}': {problem}", field.name)),
            },
        }
    }
    for key in object.keys() {
        if !output.iter().any(|field| &field.name == key) {
            problems.push(format!("unexpected field '{key}'"));
        }
    }

    if problems.is_empty() {
        Ok(TypedResult { fields })
    } else {
        Err(problems.join("; "))
    }
}

fn typed(field_type: &FieldType, value: &Value) -> Result<TypedValue, String> {
    match field_type {
        FieldType::Choice(values) => {
            let Value::String(value) = value else {
                return Err(format!("expected a string, got {}", kind_of(value)));
            };
            if values.contains(value) {
                Ok(TypedValue::Choice(value.clone()))
            } else {
                Err(format!(
                    "'{value}' is not one of {}",
                    values
                        .iter()
                        .map(|value| format!("'{value}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        }
        FieldType::Text => item(ItemType::Text, value),
        FieldType::Path => item(ItemType::Path, value),
        FieldType::Number => item(ItemType::Number, value),
        FieldType::List(item_type) => {
            let Value::Array(items) = value else {
                return Err(format!("expected a list, got {}", kind_of(value)));
            };
            items
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    item(*item_type, value).map_err(|problem| format!("item {index}: {problem}"))
                })
                .collect::<Result<Vec<_>, _>>()
                .map(TypedValue::List)
        }
    }
}

fn item(item_type: ItemType, value: &Value) -> Result<TypedValue, String> {
    match (item_type, value) {
        (ItemType::Text, Value::String(text)) => Ok(TypedValue::Text(text.clone())),
        (ItemType::Path, Value::String(path)) => {
            if is_worktree_relative(path) {
                Ok(TypedValue::Path(path.clone()))
            } else {
                Err(format!(
                    "'{path}' is not a relative path inside the worktree"
                ))
            }
        }
        (ItemType::Number, Value::Number(number)) => number
            .as_f64()
            .filter(|number| number.is_finite())
            .map(TypedValue::Number)
            .ok_or_else(|| format!("{number} is not a finite number")),
        (ItemType::Text | ItemType::Path, value) => {
            Err(format!("expected a string, got {}", kind_of(value)))
        }
        (ItemType::Number, value) => Err(format!("expected a number, got {}", kind_of(value))),
    }
}

/// Names something inside its start: not absolute, never above it with `..`, and not the start
/// itself.
fn is_worktree_relative(path: &str) -> bool {
    use std::path::{Component, Path};
    if path.is_empty() {
        return false;
    }
    let mut depth = 0usize;
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return false,
            },
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    depth > 0
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}
