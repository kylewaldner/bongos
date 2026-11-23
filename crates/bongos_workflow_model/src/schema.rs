//! Typed node output: the JSON schema handed to an agent CLI, and the hand-written check of a result
//! against the declared fields (D4). Only the five field types exist, so no validator crate is used.

use std::fmt;
use std::sync::Arc;

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

/// The schema Codex's `--output-schema` strict mode accepts: every property is `required`, and an
/// optional field is nullable (`type` is `[T, "null"]`, a choice's `enum` also lists `null`). The
/// result check treats `null` as an absent optional field.
pub fn json_schema_strict(output: &[OutputField]) -> Value {
    let properties = output
        .iter()
        .map(|field| {
            let mut schema = field_schema(&field.field_type);
            if field.optional {
                if let Some(object) = schema.as_object_mut() {
                    if let Some(kind) = object.get("type").cloned() {
                        object.insert("type".to_string(), json!([kind, "null"]));
                    }
                    if let Some(Value::Array(values)) = object.get_mut("enum") {
                        values.push(Value::Null);
                    }
                }
            }
            (field.name.clone(), schema)
        })
        .collect::<Map<_, _>>();
    let required = output
        .iter()
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

/// A result schema compiled for validation: the checked JSON Schema document (draft 2020-12), the
/// declared output fields it was built from, and the compiled validator. No `$ref` is ever fetched
/// from the network or the file system, so compiling and validating make no outside calls.
#[derive(Clone)]
pub struct OutputSchema {
    document: Value,
    fields: Vec<OutputField>,
    validator: Arc<jsonschema::Validator>,
}

impl OutputSchema {
    /// Compiles `document` as a draft 2020-12 schema. The error is the validator's description of
    /// what is wrong with the schema.
    pub fn compile(document: Value, fields: Vec<OutputField>) -> Result<OutputSchema, String> {
        let validator = jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&document)
            .map_err(|error| error.to_string())?;
        Ok(OutputSchema {
            document,
            fields,
            validator: Arc::new(validator),
        })
    }

    pub fn document(&self) -> &Value {
        &self.document
    }

    pub fn fields(&self) -> &[OutputField] {
        &self.fields
    }
}

impl fmt::Debug for OutputSchema {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputSchema")
            .field("document", &self.document)
            .field("fields", &self.fields)
            .finish_non_exhaustive()
    }
}

/// Two schemas are equal when their document and fields are; the compiled validator is derived
/// from the document and never compared.
impl PartialEq for OutputSchema {
    fn eq(&self, other: &Self) -> bool {
        self.document == other.document && self.fields == other.fields
    }
}

/// One problem found while checking a result against an [`OutputSchema`].
#[derive(Debug, Clone, PartialEq)]
pub struct CheckError {
    /// A JSON Pointer to the offending value (`/findings/2/score`); `/` is the root.
    pub path: String,
    /// The value found there; `None` when the value is absent.
    pub value: Option<Value>,
    /// What was expected, as readable text ("a string", "no such field").
    pub expected: String,
}

/// Checks `value` against `schema` and returns every problem, sorted by path.
///
/// Before validating, a top-level property whose value is `null` is dropped when the field list
/// declares it optional, or when the schema lists it under `properties` but not under `required`.
/// This lets a strict Codex schema, where an absent optional field is sent as `null`, pass. A
/// `null` for a required or undeclared property is kept and reported.
pub fn validate(schema: &OutputSchema, value: &Value) -> Result<(), Vec<CheckError>> {
    let (normalized, dropped) = drop_null_optionals(schema, value);
    let value = normalized.as_ref().unwrap_or(value);
    let mut errors = Vec::new();
    for error in schema.validator.iter_errors(value) {
        let parent = error.instance_path().as_str().to_string();
        match error.kind() {
            jsonschema::error::ValidationErrorKind::Required { property } => {
                let name = property.as_str().unwrap_or_default();
                // A strict schema requires every property; a dropped null is not missing.
                if parent.is_empty() && dropped.iter().any(|dropped| dropped == name) {
                    continue;
                }
                errors.push(CheckError {
                    path: pointer(&parent, name),
                    value: None,
                    expected: "a value".to_string(),
                });
            }
            jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected }
            | jsonschema::error::ValidationErrorKind::UnevaluatedProperties { unexpected } => {
                for name in unexpected {
                    errors.push(CheckError {
                        path: pointer(&parent, name),
                        value: error.instance().get(name.as_str()).cloned(),
                        expected: "no such field".to_string(),
                    });
                }
            }
            kind => errors.push(CheckError {
                path: root_pointer(parent),
                value: Some(error.instance().as_ref().clone()),
                expected: expected_text(kind, &error),
            }),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        errors.sort_by(|a, b| a.path.cmp(&b.path));
        Err(errors)
    }
}

/// The value without its droppable `null` properties (`None` when nothing is dropped), and the
/// names dropped.
fn drop_null_optionals(schema: &OutputSchema, value: &Value) -> (Option<Value>, Vec<String>) {
    let Some(object) = value.as_object() else {
        return (None, Vec::new());
    };
    let required = schema
        .document
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let properties = schema.document.get("properties").and_then(Value::as_object);
    let droppable = |name: &str| {
        schema
            .fields
            .iter()
            .any(|field| field.optional && field.name == name)
            || (properties.is_some_and(|properties| properties.contains_key(name))
                && !required.contains(&name))
    };
    let dropped = object
        .iter()
        .filter(|(name, value)| value.is_null() && droppable(name))
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if dropped.is_empty() {
        return (None, dropped);
    }
    let kept = object
        .iter()
        .filter(|(name, _)| !dropped.contains(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    (Some(Value::Object(kept)), dropped)
}

fn pointer(parent: &str, name: &str) -> String {
    format!("{parent}/{}", name.replace('~', "~0").replace('/', "~1"))
}

fn root_pointer(path: String) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        path
    }
}

fn expected_text(
    kind: &jsonschema::error::ValidationErrorKind,
    error: &jsonschema::ValidationError<'_>,
) -> String {
    use jsonschema::error::{TypeKind, ValidationErrorKind as Kind};
    match kind {
        Kind::Type {
            kind: TypeKind::Single(json_type),
        } => with_article(&json_type.to_string()),
        Kind::Type {
            kind: TypeKind::Multiple(types),
        } => types
            .iter()
            .map(|json_type| with_article(&json_type.to_string()))
            .collect::<Vec<_>>()
            .join(" or "),
        Kind::Enum { options } => format!(
            "one of {}",
            options
                .as_array()
                .map(|options| options.iter().map(Value::to_string).collect::<Vec<_>>())
                .unwrap_or_default()
                .join(", ")
        ),
        Kind::Constant { expected_value } => format!("the value {expected_value}"),
        Kind::Minimum { limit } => format!("at least {limit}"),
        Kind::Maximum { limit } => format!("at most {limit}"),
        Kind::ExclusiveMinimum { limit } => format!("more than {limit}"),
        Kind::ExclusiveMaximum { limit } => format!("less than {limit}"),
        Kind::MinLength { limit } => format!("at least {limit} characters"),
        Kind::MaxLength { limit } => format!("at most {limit} characters"),
        Kind::MinItems { limit } => format!("at least {limit} items"),
        Kind::MaxItems { limit } => format!("at most {limit} items"),
        Kind::Pattern { pattern } => format!("a string matching `{pattern}`"),
        Kind::UniqueItems => "unique items".to_string(),
        _ => error.to_string(),
    }
}

fn with_article(type_name: &str) -> String {
    match type_name {
        "array" => "a list".to_string(),
        "object" => "an object".to_string(),
        "integer" => "an integer".to_string(),
        "null" => "null".to_string(),
        other => format!("a {other}"),
    }
}

/// The message for a failed check: one line per error, `Result failed validation at `<path>`:
/// expected <x>, got <value>`, at most ten, then "and N more" for the rest. Callers pass errors
/// sorted by path, as [`validate`] returns them.
pub fn format_errors(errors: &[CheckError]) -> String {
    const LISTED: usize = 10;
    let mut lines = errors
        .iter()
        .take(LISTED)
        .map(|error| {
            let got = error
                .value
                .as_ref()
                .map_or_else(|| "nothing".to_string(), Value::to_string);
            format!(
                "Result failed validation at `{}`: expected {}, got {got}",
                error.path, error.expected
            )
        })
        .collect::<Vec<_>>();
    if errors.len() > LISTED {
        lines.push(format!("and {} more", errors.len() - LISTED));
    }
    lines.join("\n")
}
