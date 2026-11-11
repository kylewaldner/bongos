//! The workflow definition format: one TOML file with `schema = 1`.
//!
//! [`parse`] reads the text into a [`Definition`] and records the line of every node, route, field
//! and gate so later checks can name them. Unknown top-level, node and end keys are kept in
//! [`Definition::unknown_keys`] and reported by [`Definition::warnings`]; they are never errors, so
//! keys added by later features load today.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;

use serde::Deserialize;
use toml::Spanned;

pub const SCHEMA_VERSION: u32 = 1;

const TOP_LEVEL_KEYS: &[&str] = &[
    "schema",
    "name",
    "description",
    "start",
    "inputs",
    "defaults",
    "nodes",
    "ends",
];
const NODE_KEYS: &[&str] = &[
    "kind",
    "purpose",
    "prompt",
    "output",
    "routes",
    "join",
    "script",
    "language",
    "permissions",
    "gate",
    "use",
    "with",
    "polls",
    "agent",
    "model",
    "permission_mode",
    "allowed_tools",
    "max_turns",
    "max_cost_usd",
    "max_minutes",
    "retries",
];
const END_KEYS: &[&str] = &["kind", "outcomes", "reason"];

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

/// A value with the line it was written on.
#[derive(Debug, Clone, PartialEq)]
pub struct Located<T> {
    pub value: T,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    pub schema: u32,
    pub name: String,
    pub description: Option<String>,
    pub start: Located<String>,
    pub inputs: Vec<Input>,
    pub defaults: Settings,
    pub nodes: Vec<Node>,
    pub ends: Vec<End>,
    pub unknown_keys: Vec<UnknownKey>,
}

impl Definition {
    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.name == name)
    }

    pub fn end(&self, name: &str) -> Option<&End> {
        self.ends.iter().find(|end| end.name == name)
    }

    pub fn warnings(&self) -> Vec<Warning> {
        self.unknown_keys
            .iter()
            .map(|unknown| Warning {
                node: unknown.node.clone(),
                line: unknown.line,
                message: match &unknown.node {
                    Some(node) => format!("unknown key '{}' in '{node}'", unknown.key),
                    None => format!("unknown key '{}'", unknown.key),
                },
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    pub node: Option<String>,
    pub line: usize,
    pub message: String,
}

/// A key this version does not know, kept with its value.
#[derive(Debug, Clone, PartialEq)]
pub struct UnknownKey {
    /// The node or end the key sits in; `None` for a top-level key.
    pub node: Option<String>,
    pub key: String,
    pub line: usize,
    pub value: toml::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub name: String,
    pub line: usize,
    pub kind: InputType,
    pub from: String,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    Text,
    Path,
    Number,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
    Codex,
}

/// The keys a node may set and `[defaults]` supplies to every node.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub agent: Option<AgentKind>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub max_turns: Option<u32>,
    pub max_cost_usd: Option<f64>,
    pub max_minutes: Option<u32>,
    pub retries: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Agent,
    Program,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub name: String,
    pub line: usize,
    pub kind: Located<NodeKind>,
    pub purpose: Option<String>,
    pub prompt: Option<Located<String>>,
    pub script: Option<Located<String>>,
    pub language: Option<String>,
    /// The shipped program node named by `use`.
    pub use_node: Option<Located<String>>,
    pub with: toml::Table,
    pub permissions: Option<Permissions>,
    pub output: Vec<OutputField>,
    pub routes: Vec<Route>,
    pub join: Option<Join>,
    pub gate: Option<Gate>,
    pub polls: Option<Polls>,
    pub settings: Settings,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub commands: Vec<String>,
    pub network: Vec<String>,
    pub mcp: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Join {
    pub wait: JoinWait,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinWait {
    #[default]
    All,
    Any,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gate {
    pub line: usize,
    pub when: GateWhen,
    pub question: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateWhen {
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Polls {
    pub value: String,
    pub interval_minutes: Option<u32>,
    pub give_up_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputField {
    pub name: String,
    pub line: usize,
    pub field_type: FieldType,
    pub optional: bool,
    pub outcome: Option<Outcome>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Choice(Vec<String>),
    Text,
    Path,
    Number,
    List(ItemType),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemType {
    Text,
    Path,
    Number,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pr,
    Document,
    Tasks,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub line: usize,
    pub when: Vec<Condition>,
    pub default: bool,
    pub to: Option<Target>,
    pub fan_out: Option<FanOut>,
    pub max_passes: Option<u32>,
    pub on_bound: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub field: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Target {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FanOut {
    pub over: String,
    pub to: String,
    pub width: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct End {
    pub name: String,
    pub line: usize,
    pub kind: EndKind,
    pub outcomes: Vec<Outcome>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndKind {
    Success,
    Failure,
}

#[derive(Deserialize)]
struct RawDefinition {
    schema: Spanned<u32>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    start: Spanned<String>,
    #[serde(default)]
    inputs: HashMap<String, Spanned<RawInput>>,
    #[serde(default)]
    defaults: RawSettings,
    #[serde(default)]
    nodes: HashMap<String, Spanned<RawNode>>,
    #[serde(default)]
    ends: HashMap<String, Spanned<RawEnd>>,
}

#[derive(Deserialize)]
struct RawInput {
    #[serde(rename = "type")]
    kind: InputType,
    from: String,
    #[serde(default = "default_true")]
    required: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Default, Deserialize)]
struct RawSettings {
    agent: Option<AgentKind>,
    model: Option<String>,
    permission_mode: Option<String>,
    allowed_tools: Option<Vec<String>>,
    max_turns: Option<u32>,
    max_cost_usd: Option<f64>,
    max_minutes: Option<u32>,
    retries: Option<u32>,
}

impl From<RawSettings> for Settings {
    fn from(raw: RawSettings) -> Self {
        Self {
            agent: raw.agent,
            model: raw.model,
            permission_mode: raw.permission_mode,
            allowed_tools: raw.allowed_tools,
            max_turns: raw.max_turns,
            max_cost_usd: raw.max_cost_usd,
            max_minutes: raw.max_minutes,
            retries: raw.retries,
        }
    }
}

#[derive(Deserialize)]
struct RawNode {
    kind: Spanned<NodeKind>,
    purpose: Option<String>,
    prompt: Option<Spanned<String>>,
    script: Option<Spanned<String>>,
    language: Option<String>,
    #[serde(rename = "use")]
    use_node: Option<Spanned<String>>,
    #[serde(default)]
    with: toml::Table,
    permissions: Option<Permissions>,
    #[serde(default)]
    output: HashMap<String, Spanned<RawField>>,
    #[serde(default)]
    routes: Vec<Spanned<RawRoute>>,
    join: Option<Join>,
    gate: Option<Spanned<RawGate>>,
    polls: Option<Polls>,
    agent: Option<AgentKind>,
    model: Option<String>,
    permission_mode: Option<String>,
    allowed_tools: Option<Vec<String>>,
    max_turns: Option<u32>,
    max_cost_usd: Option<f64>,
    max_minutes: Option<u32>,
    retries: Option<u32>,
}

#[derive(Deserialize)]
struct RawGate {
    when: GateWhen,
    question: String,
}

#[derive(Deserialize)]
struct RawField {
    #[serde(rename = "type")]
    field_type: RawFieldType,
    values: Option<Vec<String>>,
    of: Option<ItemType>,
    #[serde(default)]
    optional: bool,
    outcome: Option<Outcome>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawFieldType {
    Choice,
    Text,
    Path,
    Number,
    List,
}

#[derive(Deserialize)]
struct RawRoute {
    #[serde(default)]
    when: BTreeMap<String, String>,
    #[serde(default)]
    default: bool,
    to: Option<Target>,
    fan_out: Option<FanOut>,
    max_passes: Option<u32>,
    on_bound: Option<String>,
}

#[derive(Deserialize)]
struct RawEnd {
    kind: EndKind,
    #[serde(default)]
    outcomes: Vec<Outcome>,
    reason: Option<String>,
}

struct LineIndex {
    newlines: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let newlines = text
            .bytes()
            .enumerate()
            .filter_map(|(offset, byte)| (byte == b'\n').then_some(offset))
            .collect();
        Self { newlines }
    }

    fn line(&self, offset: usize) -> usize {
        self.newlines.partition_point(|&newline| newline < offset) + 1
    }

    fn line_of<T>(&self, spanned: &Spanned<T>) -> usize {
        self.line(spanned.span().start)
    }

    fn error(&self, span: Option<Range<usize>>, message: impl Into<String>) -> ParseError {
        ParseError {
            line: span.map_or(1, |span| self.line(span.start)),
            message: message.into(),
        }
    }
}

/// Parses a definition file. Errors name the line they were found on.
pub fn parse(text: &str) -> Result<Definition, ParseError> {
    let lines = LineIndex::new(text);
    let raw: RawDefinition =
        toml::from_str(text).map_err(|error| lines.error(error.span(), error.message()))?;

    if raw.schema.get_ref() != &SCHEMA_VERSION {
        return Err(lines.error(
            Some(raw.schema.span()),
            format!(
                "unsupported schema {}; this version reads schema {SCHEMA_VERSION}",
                raw.schema.get_ref()
            ),
        ));
    }

    let table: toml::Table =
        toml::from_str(text).map_err(|error| lines.error(error.span(), error.message()))?;
    let unknown_keys = unknown_keys(text, &table, &lines)?;

    let mut inputs = raw
        .inputs
        .into_iter()
        .map(|(name, input)| {
            let line = lines.line_of(&input);
            (
                input.span().start,
                Input {
                    name,
                    line,
                    kind: input.get_ref().kind,
                    from: input.get_ref().from.clone(),
                    required: input.get_ref().required,
                },
            )
        })
        .collect::<Vec<_>>();
    inputs.sort_by_key(|(start, _)| *start);

    let mut nodes = Vec::new();
    for (name, node) in raw.nodes {
        let start = node.span().start;
        nodes.push((start, convert_node(name, node, &lines)?));
    }
    nodes.sort_by_key(|(start, _)| *start);

    let mut ends = raw
        .ends
        .into_iter()
        .map(|(name, end)| {
            let line = lines.line_of(&end);
            let start = end.span().start;
            let end = end.into_inner();
            (
                start,
                End {
                    name,
                    line,
                    kind: end.kind,
                    outcomes: end.outcomes,
                    reason: end.reason,
                },
            )
        })
        .collect::<Vec<_>>();
    ends.sort_by_key(|(start, _)| *start);

    Ok(Definition {
        schema: *raw.schema.get_ref(),
        name: raw.name,
        description: raw.description,
        start: Located {
            line: lines.line_of(&raw.start),
            value: raw.start.into_inner(),
        },
        inputs: inputs.into_iter().map(|(_, input)| input).collect(),
        defaults: raw.defaults.into(),
        nodes: nodes.into_iter().map(|(_, node)| node).collect(),
        ends: ends.into_iter().map(|(_, end)| end).collect(),
        unknown_keys,
    })
}

fn located<T>(spanned: Spanned<T>, lines: &LineIndex) -> Located<T> {
    Located {
        line: lines.line_of(&spanned),
        value: spanned.into_inner(),
    }
}

fn convert_node(
    name: String,
    node: Spanned<RawNode>,
    lines: &LineIndex,
) -> Result<Node, ParseError> {
    let line = lines.line_of(&node);
    let raw = node.into_inner();

    let mut output = raw
        .output
        .into_iter()
        .map(|(field, spec)| {
            let start = spec.span().start;
            convert_field(field, spec, lines).map(|field| (start, field))
        })
        .collect::<Result<Vec<_>, _>>()?;
    output.sort_by_key(|(start, _)| *start);

    let routes = raw
        .routes
        .into_iter()
        .map(|route| {
            let line = lines.line_of(&route);
            let route = route.into_inner();
            Route {
                line,
                when: route
                    .when
                    .into_iter()
                    .map(|(field, value)| Condition { field, value })
                    .collect(),
                default: route.default,
                to: route.to,
                fan_out: route.fan_out,
                max_passes: route.max_passes,
                on_bound: route.on_bound,
            }
        })
        .collect();

    Ok(Node {
        name,
        line,
        kind: located(raw.kind, lines),
        purpose: raw.purpose,
        prompt: raw.prompt.map(|prompt| located(prompt, lines)),
        script: raw.script.map(|script| located(script, lines)),
        language: raw.language,
        use_node: raw.use_node.map(|use_node| located(use_node, lines)),
        with: raw.with,
        permissions: raw.permissions,
        output: output.into_iter().map(|(_, field)| field).collect(),
        routes,
        join: raw.join,
        gate: raw.gate.map(|gate| {
            let line = lines.line_of(&gate);
            let gate = gate.into_inner();
            Gate {
                line,
                when: gate.when,
                question: gate.question,
            }
        }),
        polls: raw.polls,
        settings: Settings {
            agent: raw.agent,
            model: raw.model,
            permission_mode: raw.permission_mode,
            allowed_tools: raw.allowed_tools,
            max_turns: raw.max_turns,
            max_cost_usd: raw.max_cost_usd,
            max_minutes: raw.max_minutes,
            retries: raw.retries,
        },
    })
}

fn convert_field(
    name: String,
    spec: Spanned<RawField>,
    lines: &LineIndex,
) -> Result<OutputField, ParseError> {
    let line = lines.line_of(&spec);
    let spec = spec.into_inner();
    let field_type = match spec.field_type {
        RawFieldType::Choice => match spec.values {
            Some(values) if !values.is_empty() => FieldType::Choice(values),
            _ => {
                return Err(ParseError {
                    line,
                    message: format!("choice field '{name}' needs a non-empty `values`"),
                });
            }
        },
        RawFieldType::Text => FieldType::Text,
        RawFieldType::Path => FieldType::Path,
        RawFieldType::Number => FieldType::Number,
        RawFieldType::List => match spec.of {
            Some(item) => FieldType::List(item),
            None => {
                return Err(ParseError {
                    line,
                    message: format!("list field '{name}' needs `of`"),
                });
            }
        },
    };
    Ok(OutputField {
        name,
        line,
        field_type,
        optional: spec.optional,
        outcome: spec.outcome,
    })
}

fn unknown_keys(
    text: &str,
    values: &toml::Table,
    lines: &LineIndex,
) -> Result<Vec<UnknownKey>, ParseError> {
    let document = toml_edit::ImDocument::parse(text)
        .map_err(|error| lines.error(error.span(), error.message()))?;
    let root = document.as_table();

    let mut unknown = Vec::new();
    collect_unknown(
        root,
        TOP_LEVEL_KEYS,
        None,
        Some(values),
        lines,
        &mut unknown,
    );
    for (section, known) in [("nodes", NODE_KEYS), ("ends", END_KEYS)] {
        let Some(members) = root.get(section).and_then(toml_edit::Item::as_table_like) else {
            continue;
        };
        for (name, item) in members.iter() {
            let Some(member) = item.as_table_like() else {
                continue;
            };
            let member_values = values
                .get(section)
                .and_then(|section| section.get(name))
                .and_then(toml::Value::as_table);
            collect_unknown(
                member,
                known,
                Some(name),
                member_values,
                lines,
                &mut unknown,
            );
        }
    }
    unknown.sort_by_key(|key| key.line);
    Ok(unknown)
}

fn collect_unknown(
    table: &dyn toml_edit::TableLike,
    known: &[&str],
    node: Option<&str>,
    values: Option<&toml::Table>,
    lines: &LineIndex,
    out: &mut Vec<UnknownKey>,
) {
    for (key, item) in table.iter() {
        if known.contains(&key) {
            continue;
        }
        let span = table
            .get_key_value(key)
            .and_then(|(key, _)| key.span())
            .or_else(|| item.span());
        let value = values
            .and_then(|values| values.get(key))
            .cloned()
            .unwrap_or_else(|| toml::Value::String(item.to_string().trim().to_string()));
        out.push(UnknownKey {
            node: node.map(str::to_string),
            key: key.to_string(),
            line: span.map_or(1, |span| lines.line(span.start)),
            value,
        });
    }
}
