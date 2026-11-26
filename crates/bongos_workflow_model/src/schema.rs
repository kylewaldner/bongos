//! Typed node output: the JSON schema handed to an agent CLI, and the hand-written check of a result
//! against the declared fields (D4). Only the five field types exist, so no validator crate is used.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::definition::{FieldType, ItemType, Node, Outcome, OutputField};
use crate::validate::{Diagnostic, Severity};

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

/// The schema Codex's `--output-schema` strict mode accepts, derived from a node's schema
/// `document`: every property is `required`, and a property the document does not require is
/// nullable (`type` is `[T, "null"]`, an `enum` also lists `null`). The result check treats `null`
/// as an absent optional field.
pub fn json_schema_strict(document: &Value) -> Value {
    let mut strict = document.clone();
    let Some(root) = strict.as_object_mut() else {
        return strict;
    };
    let required = root
        .get("required")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut names = Vec::new();
    if let Some(Value::Object(properties)) = root.get_mut("properties") {
        for (name, schema) in properties.iter_mut() {
            names.push(Value::String(name.clone()));
            if required.contains(name) {
                continue;
            }
            if let Some(object) = schema.as_object_mut() {
                match object.get("type").cloned() {
                    Some(Value::Array(mut kinds)) => {
                        if !kinds.contains(&json!("null")) {
                            kinds.push(json!("null"));
                        }
                        object.insert("type".to_string(), Value::Array(kinds));
                    }
                    Some(kind) => {
                        object.insert("type".to_string(), json!([kind, "null"]));
                    }
                    None => {}
                }
                if let Some(Value::Array(values)) = object.get_mut("enum")
                    && !values.contains(&Value::Null)
                {
                    values.push(Value::Null);
                }
            }
        }
    }
    root.insert("required".to_string(), Value::Array(names));
    strict
}

fn field_schema(field_type: &FieldType) -> Value {
    match field_type {
        FieldType::Choice(values) => json!({ "type": "string", "enum": values }),
        FieldType::Text | FieldType::Path => json!({ "type": "string" }),
        FieldType::Number => json!({ "type": "number" }),
        FieldType::List(item) => json!({ "type": "array", "items": item_schema(*item) }),
        FieldType::Data => json!({}),
        FieldType::GraphEdits => graph_edits_schema(),
    }
}

/// The schema of a `graph_edits` field: `{ "project"?, "ops": [op] }` where `op` is one of the
/// graph operations.
fn graph_edits_schema() -> Value {
    let text = json!({ "type": "string", "minLength": 1 });
    let texts = json!({ "type": "array", "items": { "type": "string" } });
    let kind = json!({ "type": "string", "enum": ["after", "stacked_on"] });
    let op = |name: &str, properties: Value, required: &[&str]| {
        let mut properties = properties.as_object().cloned().unwrap_or_default();
        properties.insert("op".to_string(), json!({ "const": name }));
        let mut required = required.to_vec();
        required.push("op");
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    };
    json!({
        "type": "object",
        "properties": {
            "project": { "type": "string" },
            "ops": {
                "type": "array",
                "items": { "oneOf": [
                    op("create", json!({
                        "ref": text,
                        "parent": text,
                        "goal": text,
                        "work_kind": { "type": "string" },
                        "areas": texts,
                        "size": { "type": "integer", "minimum": 0 },
                        "repositories": texts,
                        "todos": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "workflow": { "type": "string" },
                                },
                                "required": ["name"],
                                "additionalProperties": false,
                            },
                        },
                    }), &["parent", "goal"]),
                    op("set", json!({
                        "task": text,
                        "goal": text,
                        "work_kind": { "type": "string" },
                        "size": { "type": "integer", "minimum": 0 },
                        "areas": texts,
                    }), &["task"]),
                    op("move", json!({ "task": text, "parent": text }), &["task", "parent"]),
                    op("remove", json!({ "task": text }), &["task"]),
                    op("add_edge", json!({
                        "from": text,
                        "to": text,
                        "kind": kind,
                        "unblock": { "type": "string" },
                    }), &["from", "to", "kind"]),
                    op("remove_edge", json!({
                        "from": text,
                        "to": text,
                        "kind": kind,
                    }), &["from", "to", "kind"]),
                ] },
            },
        },
        "required": ["ops"],
        "additionalProperties": false,
    })
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
    /// A value of a file schema's field that is none of the other types; kept as given.
    Data(Value),
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
            TypedValue::Data(value) => value.clone(),
        }
    }

    /// The string of a choice, text or path value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            TypedValue::Choice(value) | TypedValue::Text(value) | TypedValue::Path(value) => {
                Some(value)
            }
            TypedValue::Number(_) | TypedValue::List(_) | TypedValue::Data(_) => None,
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

/// Checks a result against a node's schema: [`validate`] first (every problem listed, sorted by
/// path, see [`format_errors`]), then the conversion with the schema's field list into a
/// [`TypedResult`]. An optional field that is absent or `null` is not listed. A `Path` value must be
/// relative and stay inside the worktree; that rule is a post-check reported as a [`CheckError`].
pub fn check(schema: &OutputSchema, value: &Value) -> Result<TypedResult, String> {
    if let Err(errors) = validate(schema, value) {
        return Err(format_errors(&errors));
    }
    let Value::Object(object) = value else {
        return Err(format_errors(&[CheckError {
            path: "/".to_string(),
            value: Some(value.clone()),
            expected: "an object".to_string(),
        }]));
    };
    let mut errors = Vec::new();
    let mut fields = Vec::new();
    for field in schema.fields() {
        let name = pointer("", &field.name);
        match object.get(&field.name) {
            None | Some(Value::Null) => {}
            Some(value) => match typed(&field.field_type, value, &name) {
                Ok(typed) => fields.push(TypedField {
                    name: field.name.clone(),
                    value: typed,
                    outcome: field.outcome,
                }),
                Err(error) => errors.push(error),
            },
        }
    }
    if errors.is_empty() {
        Ok(TypedResult { fields })
    } else {
        errors.sort_by(|a, b| a.path.cmp(&b.path));
        Err(format_errors(&errors))
    }
}

/// Converts a value the validator accepted. A value that does not fit its declared type (a schema
/// whose document and field list disagree) is reported as a [`CheckError`] too.
fn typed(field_type: &FieldType, value: &Value, path: &str) -> Result<TypedValue, CheckError> {
    let wrong = |expected: &str| CheckError {
        path: path.to_string(),
        value: Some(value.clone()),
        expected: expected.to_string(),
    };
    match field_type {
        FieldType::Choice(_) => match value {
            Value::String(value) => Ok(TypedValue::Choice(value.clone())),
            _ => Err(wrong("a string")),
        },
        FieldType::Text => item(ItemType::Text, value, path),
        FieldType::Path => item(ItemType::Path, value, path),
        FieldType::Number => item(ItemType::Number, value, path),
        FieldType::Data | FieldType::GraphEdits => Ok(TypedValue::Data(value.clone())),
        FieldType::List(item_type) => {
            let Value::Array(items) = value else {
                return Err(wrong("a list"));
            };
            items
                .iter()
                .enumerate()
                .map(|(index, value)| item(*item_type, value, &format!("{path}/{index}")))
                .collect::<Result<Vec<_>, _>>()
                .map(TypedValue::List)
        }
    }
}

fn item(item_type: ItemType, value: &Value, path: &str) -> Result<TypedValue, CheckError> {
    let wrong = |expected: &str| CheckError {
        path: path.to_string(),
        value: Some(value.clone()),
        expected: expected.to_string(),
    };
    match (item_type, value) {
        (ItemType::Text, Value::String(text)) => Ok(TypedValue::Text(text.clone())),
        (ItemType::Path, Value::String(text)) => {
            if is_worktree_relative(text) {
                Ok(TypedValue::Path(text.clone()))
            } else {
                Err(wrong("a relative path inside the worktree"))
            }
        }
        (ItemType::Number, Value::Number(number)) => number
            .as_f64()
            .filter(|number| number.is_finite())
            .map(TypedValue::Number)
            .ok_or_else(|| wrong("a finite number")),
        (ItemType::Text | ItemType::Path, _) => Err(wrong("a string")),
        (ItemType::Number, _) => Err(wrong("a number")),
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

    /// The schema generated from declared `output` fields (the document of [`json_schema`]).
    pub fn from_fields(fields: &[OutputField]) -> Result<OutputSchema, String> {
        OutputSchema::compile(json_schema(fields), fields.to_vec())
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

/// Checks `value` against `schema` and returns the first problem of each value, sorted by path.
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
        // One value can fail several keywords (`type` and `enum`); the first is enough.
        errors.dedup_by(|a, b| a.path == b.path);
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

/// The JSON Schema dialect a file schema may name in `$schema`.
const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Keywords a file schema may use; every other keyword is a load error.
const ALLOWED_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "additionalProperties",
    "title",
    "description",
    "$schema",
];
const ALLOWED_TYPES: &[&str] = &[
    "string", "number", "integer", "boolean", "array", "object", "null",
];

/// The one schema document of a node's result type, compiled. A node with `output` gets the
/// document [`json_schema`] generates; a node with `output_schema` gets the file in `dir`, read
/// here, checked against the keyword allowlist and mapped to fields (its top-level properties; a
/// value that is not a string, choice, number or list of strings or numbers is a
/// [`FieldType::Data`]). This is the only place a schema file is read, at load.
pub fn document(node: &Node, dir: &Path) -> Result<OutputSchema, Diagnostic> {
    let fail = |line: usize, message: String| Diagnostic {
        node: Some(node.name.clone()),
        line,
        severity: Severity::Error,
        message,
    };
    let Some(file) = &node.output_schema else {
        return OutputSchema::from_fields(&node.output).map_err(|message| {
            fail(
                node.line,
                format!("the output schema is invalid: {message}"),
            )
        });
    };
    let line = file.line;
    if !node.output.is_empty() {
        return Err(fail(
            line,
            "set either `output` or `output_schema`, not both".to_string(),
        ));
    }
    if !is_worktree_relative(&file.value) {
        return Err(fail(
            line,
            format!(
                "output_schema '{}' must be a relative path inside the definition directory",
                file.value
            ),
        ));
    }
    let text = std::fs::read_to_string(dir.join(&file.value)).map_err(|error| {
        fail(
            line,
            format!("cannot read output_schema '{}': {error}", file.value),
        )
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        fail(
            line,
            format!("output_schema '{}' is not valid JSON: {error}", file.value),
        )
    })?;
    let mut problems = Vec::new();
    check_keywords(&value, "", &mut problems);
    if let Some(problem) = problems.into_iter().next() {
        return Err(fail(
            line,
            format!("output_schema '{}': {problem}", file.value),
        ));
    }
    let mut fields = fields_of(&value)
        .map_err(|problem| fail(line, format!("output_schema '{}': {problem}", file.value)))?;
    for field in &mut fields {
        field.line = line;
    }
    OutputSchema::compile(value, fields).map_err(|message| {
        fail(
            line,
            format!("output_schema '{}' is invalid: {message}", file.value),
        )
    })
}

/// Lists every keyword of `schema` outside the allowlist as "keyword 'k' is not allowed at <path>"
/// (the path is a JSON Pointer to the keyword), and every malformed use of an allowed one.
fn check_keywords(schema: &Value, path: &str, problems: &mut Vec<String>) {
    let at = |path: &str| {
        if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        }
    };
    let Value::Object(object) = schema else {
        problems.push(format!("a schema must be an object, at {}", at(path)));
        return;
    };
    for (keyword, value) in object {
        let here = format!("{path}/{}", keyword.replace('~', "~0").replace('/', "~1"));
        if !ALLOWED_KEYWORDS.contains(&keyword.as_str()) {
            problems.push(format!("keyword '{keyword}' is not allowed at {here}"));
            continue;
        }
        match keyword.as_str() {
            "properties" => match value {
                Value::Object(properties) => {
                    for (name, property) in properties {
                        let name = name.replace('~', "~0").replace('/', "~1");
                        check_keywords(property, &format!("{here}/{name}"), problems);
                    }
                }
                _ => problems.push(format!("'properties' must be an object, at {here}")),
            },
            "items" => check_keywords(value, &here, problems),
            "additionalProperties" if value != &Value::Bool(false) => problems.push(format!(
                "'additionalProperties' may only be false, at {here}"
            )),
            "$schema" if value.as_str() != Some(DRAFT_2020_12) => {
                problems.push(format!("'$schema' must be {DRAFT_2020_12}, at {here}"))
            }
            "type" => {
                let names = match value {
                    Value::String(name) => vec![name.as_str()],
                    Value::Array(names) => names.iter().filter_map(Value::as_str).collect(),
                    _ => Vec::new(),
                };
                if names.is_empty() || names.iter().any(|name| !ALLOWED_TYPES.contains(name)) {
                    problems.push(format!("'type' names an unsupported type, at {here}"));
                }
            }
            _ => {}
        }
    }
}

/// The fields a file schema declares: one per top-level property, optional when not `required`.
fn fields_of(document: &Value) -> Result<Vec<OutputField>, String> {
    if document.get("type").and_then(Value::as_str) != Some("object") {
        return Err("the top level must have \"type\": \"object\"".to_string());
    }
    let required = document
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let Some(properties) = document.get("properties").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    Ok(properties
        .iter()
        .map(|(name, schema)| OutputField {
            name: name.clone(),
            line: 0,
            field_type: field_type_of(schema),
            optional: !required.contains(&name.as_str()),
            outcome: None,
        })
        .collect())
}

fn field_type_of(schema: &Value) -> FieldType {
    let kind = schema.get("type").and_then(Value::as_str);
    match kind {
        Some("string") => match schema.get("enum").and_then(Value::as_array) {
            Some(values) => {
                let values = values
                    .iter()
                    .map(|value| value.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>();
                match values {
                    Some(values) if !values.is_empty() => FieldType::Choice(values),
                    _ => FieldType::Data,
                }
            }
            None => FieldType::Text,
        },
        Some("number" | "integer") => FieldType::Number,
        Some("array") => match schema
            .get("items")
            .and_then(|items| items.get("type"))
            .and_then(Value::as_str)
        {
            Some("string") => FieldType::List(ItemType::Text),
            Some("number" | "integer") => FieldType::List(ItemType::Number),
            _ => FieldType::Data,
        },
        _ => FieldType::Data,
    }
}
