//! The workflow definition format: one TOML file with `schema = 1`.
//!
//! [`parse`] reads the text into a [`Definition`] and records the line of every node, route, field
//! and gate so later checks can name them. Unknown top-level, node and end keys are kept in
//! [`Definition::unknown_keys`] and reported by [`Definition::warnings`]; they are never errors, so
//! keys added by later features load today.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;

use serde::Deserialize;
use toml::Spanned;

use crate::schema::OutputSchema;

pub const SCHEMA_VERSION: u32 = 1;

const TOP_LEVEL_KEYS: &[&str] = &[
    "schema",
    "name",
    "description",
    "start",
    "dispatcher",
    "instances",
    "inputs",
    "defaults",
    "nodes",
    "ends",
    "graph_edits",
];
const NODE_KEYS: &[&str] = &[
    "kind",
    "purpose",
    "prompt",
    "output",
    "output_schema",
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

/// The shipped program node that runs a task's checks, named by `use`.
pub const CHECKS_PROGRAM: &str = "checks";
/// The `verdict` values of a checks node's fixed output.
pub const CHECKS_VERDICTS: [&str; 3] = ["pass", "fail", "no_verdict"];
/// The verdict of a checks node that could not decide; unrouted, it holds the run.
pub const NO_VERDICT: &str = "no_verdict";

/// The fixed output of a checks node, every field on `line`. A checks node that declares no
/// `output` gets this one when parsed; a declared one must equal it.
pub fn checks_output(line: usize) -> Vec<OutputField> {
    let field = |name: &str, field_type: FieldType| OutputField {
        name: name.to_string(),
        line,
        field_type,
        optional: false,
        outcome: None,
    };
    vec![
        field(
            "verdict",
            FieldType::Choice(CHECKS_VERDICTS.map(String::from).to_vec()),
        ),
        field("failing", FieldType::List(ItemType::Text)),
        field("scope", FieldType::Text),
        field("evidence", FieldType::List(ItemType::Text)),
        field("summary", FieldType::Text),
    ]
}

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
    /// The first node. Always set on a definition that is not a dispatcher (parse requires it);
    /// a dispatcher has none.
    pub start: Option<Located<String>>,
    /// A dispatcher definition: it declares no nodes or ends and is never driven by the scheduler.
    pub dispatcher: bool,
    /// How many runs of this definition may be active at once in a project; `None` is no limit.
    /// A dispatcher's is always 1 (a written 0 is kept so validation reports it).
    pub instances: Option<u32>,
    /// The line `instances` was written on.
    pub instances_line: Option<usize>,
    pub inputs: Vec<Input>,
    pub defaults: Settings,
    pub nodes: Vec<Node>,
    pub ends: Vec<End>,
    pub unknown_keys: Vec<UnknownKey>,
    /// The `[graph_edits]` table; `None` when absent, which means [`Scope::None`].
    pub graph_edits: Option<GraphEditsDecl>,
}

/// Which tasks a workflow's graph edits may touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// No graph edits (the default).
    None,
    /// The run's task and its descendants.
    OwnSubtree,
    /// Every task of the run's project.
    OwnProject,
    /// Exactly the projects listed in `projects`.
    Projects,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::None => "none",
            Scope::OwnSubtree => "own-subtree",
            Scope::OwnProject => "own-project",
            Scope::Projects => "projects",
        }
    }
}

/// When a node's graph edits land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandAt {
    /// At the end of each node (the default).
    Step,
    /// Once, when the run reaches a success end.
    RunEnd,
}

impl LandAt {
    pub fn as_str(self) -> &'static str {
        match self {
            LandAt::Step => "step",
            LandAt::RunEnd => "run-end",
        }
    }
}

/// The parsed `[graph_edits]` table.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphEditsDecl {
    /// The line of the `[graph_edits]` header.
    pub line: usize,
    pub scope: Located<Scope>,
    pub projects: Vec<Located<String>>,
    pub land_at: LandAt,
}

impl GraphEditsDecl {
    /// The sha256 (lowercase hex) of the table's canonical text, which does not depend on
    /// spacing, comments or key order in the file.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write;

        let mut text = format!("[graph_edits]\nscope = {:?}\n", self.scope.value.as_str());
        if !self.projects.is_empty() {
            let projects = self
                .projects
                .iter()
                .map(|project| format!("{:?}", project.value))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(text, "projects = [{projects}]");
        }
        let _ = writeln!(text, "land_at = {:?}", self.land_at.as_str());
        Sha256::digest(text.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }
}

impl Definition {
    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|node| node.name == name)
    }

    pub fn end(&self, name: &str) -> Option<&End> {
        self.ends.iter().find(|end| end.name == name)
    }

    /// The name of the first node; empty for a dispatcher.
    pub fn start_node(&self) -> &str {
        self.start.as_ref().map_or("", |start| start.value.as_str())
    }

    /// The line `start` was written on; 1 when there is none.
    pub fn start_line(&self) -> usize {
        self.start.as_ref().map_or(1, |start| start.line)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
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
    /// The line each key was written on, for diagnostics.
    pub lines: SettingLines,
}

/// The line of each key of [`Settings`] that was set; `allowed_tools` has one line per entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingLines {
    pub agent: Option<usize>,
    pub model: Option<usize>,
    pub permission_mode: Option<usize>,
    pub allowed_tools: Vec<usize>,
    pub max_turns: Option<usize>,
    pub max_cost_usd: Option<usize>,
    pub max_minutes: Option<usize>,
    pub retries: Option<usize>,
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
    /// `output_schema`: a schema file in the definition directory, instead of `output`.
    pub output_schema: Option<Located<String>>,
    /// The node's one result schema document, compiled at load. A node with `output_schema` holds
    /// an empty placeholder until the loader reads the file ([`crate::schema::document`]).
    pub schema: Arc<OutputSchema>,
    pub routes: Vec<Route>,
    pub join: Option<Join>,
    pub gate: Option<Gate>,
    pub polls: Option<Polls>,
    pub settings: Settings,
}

impl Node {
    /// Whether this is a program node running the shipped checks program.
    pub fn is_checks(&self) -> bool {
        self.kind.value == NodeKind::Program
            && self
                .use_node
                .as_ref()
                .is_some_and(|use_node| use_node.value == CHECKS_PROGRAM)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub commands: Vec<String>,
    pub network: Vec<String>,
    pub mcp: Vec<String>,
    /// The line `permissions` was written on.
    #[serde(skip)]
    pub line: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Join {
    pub wait: JoinWait,
    /// The line `join` was written on.
    #[serde(skip)]
    pub line: usize,
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
    /// Any other value of a file schema, kept as JSON in `result.json`: never routable, never an
    /// outcome and not editable. It cannot be declared in `output`.
    Data,
    /// A batch of graph operations (`{ "project"?, "ops": [...] }`), kept as JSON. It is never
    /// routable, an outcome or editable.
    GraphEdits,
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
    #[serde(default)]
    start: Option<Spanned<String>>,
    #[serde(default)]
    dispatcher: bool,
    #[serde(default)]
    instances: Option<Spanned<u32>>,
    #[serde(default)]
    inputs: HashMap<String, Spanned<RawInput>>,
    #[serde(default)]
    defaults: RawSettings,
    #[serde(default)]
    nodes: HashMap<String, Spanned<RawNode>>,
    #[serde(default)]
    ends: HashMap<String, Spanned<RawEnd>>,
    #[serde(default)]
    graph_edits: Option<Spanned<RawGraphEdits>>,
}

#[derive(Deserialize)]
struct RawGraphEdits {
    #[serde(default)]
    scope: Option<Spanned<String>>,
    #[serde(default)]
    projects: Option<Vec<Spanned<String>>>,
    #[serde(default)]
    land_at: Option<Spanned<String>>,
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
    agent: Option<Spanned<AgentKind>>,
    model: Option<Spanned<String>>,
    permission_mode: Option<Spanned<String>>,
    allowed_tools: Option<Vec<Spanned<String>>>,
    max_turns: Option<Spanned<u32>>,
    max_cost_usd: Option<Spanned<f64>>,
    max_minutes: Option<Spanned<u32>>,
    retries: Option<Spanned<u32>>,
}

impl RawSettings {
    fn convert(self, lines: &LineIndex) -> Settings {
        fn split<T>(value: Option<Spanned<T>>, lines: &LineIndex) -> (Option<T>, Option<usize>) {
            match value {
                Some(value) => {
                    let line = lines.line_of(&value);
                    (Some(value.into_inner()), Some(line))
                }
                None => (None, None),
            }
        }
        let (agent, agent_line) = split(self.agent, lines);
        let (model, model_line) = split(self.model, lines);
        let (permission_mode, permission_mode_line) = split(self.permission_mode, lines);
        let (max_turns, max_turns_line) = split(self.max_turns, lines);
        let (max_cost_usd, max_cost_usd_line) = split(self.max_cost_usd, lines);
        let (max_minutes, max_minutes_line) = split(self.max_minutes, lines);
        let (retries, retries_line) = split(self.retries, lines);
        let tool_lines = self
            .allowed_tools
            .iter()
            .flatten()
            .map(|tool| lines.line_of(tool))
            .collect();
        let allowed_tools = self
            .allowed_tools
            .map(|tools| tools.into_iter().map(Spanned::into_inner).collect());
        Settings {
            agent,
            model,
            permission_mode,
            allowed_tools,
            max_turns,
            max_cost_usd,
            max_minutes,
            retries,
            lines: SettingLines {
                agent: agent_line,
                model: model_line,
                permission_mode: permission_mode_line,
                allowed_tools: tool_lines,
                max_turns: max_turns_line,
                max_cost_usd: max_cost_usd_line,
                max_minutes: max_minutes_line,
                retries: retries_line,
            },
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
    permissions: Option<Spanned<Permissions>>,
    #[serde(default)]
    output: HashMap<String, Spanned<RawField>>,
    output_schema: Option<Spanned<String>>,
    #[serde(default)]
    routes: Vec<Spanned<RawRoute>>,
    join: Option<Spanned<Join>>,
    gate: Option<Spanned<RawGate>>,
    polls: Option<Polls>,
    agent: Option<Spanned<AgentKind>>,
    model: Option<Spanned<String>>,
    permission_mode: Option<Spanned<String>>,
    allowed_tools: Option<Vec<Spanned<String>>>,
    max_turns: Option<Spanned<u32>>,
    max_cost_usd: Option<Spanned<f64>>,
    max_minutes: Option<Spanned<u32>>,
    retries: Option<Spanned<u32>>,
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
    #[serde(rename = "graph_edits")]
    GraphEdits,
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

    if raw.start.is_none() && !raw.dispatcher {
        return Err(lines.error(None, "missing field `start`"));
    }
    let instances_line = raw.instances.as_ref().map(|value| lines.line_of(value));
    let mut instances = raw.instances.map(Spanned::into_inner);
    // "A lane-using workflow is limited to one": a dispatcher's limit is 1 whatever is written,
    // but a written 0 stays so validation reports it.
    if raw.dispatcher && instances != Some(0) {
        instances = Some(1);
    }

    let graph_edits = raw
        .graph_edits
        .map(|decl| convert_graph_edits(decl, &lines))
        .transpose()?;

    Ok(Definition {
        schema: *raw.schema.get_ref(),
        name: raw.name,
        description: raw.description,
        start: raw.start.map(|start| located(start, &lines)),
        dispatcher: raw.dispatcher,
        instances,
        instances_line,
        inputs: inputs.into_iter().map(|(_, input)| input).collect(),
        defaults: raw.defaults.convert(&lines),
        nodes: nodes.into_iter().map(|(_, node)| node).collect(),
        ends: ends.into_iter().map(|(_, end)| end).collect(),
        unknown_keys,
        graph_edits,
    })
}

fn convert_graph_edits(
    decl: Spanned<RawGraphEdits>,
    lines: &LineIndex,
) -> Result<GraphEditsDecl, ParseError> {
    let line = lines.line_of(&decl);
    let raw = decl.into_inner();
    let scope = match raw.scope {
        None => Located {
            value: Scope::None,
            line,
        },
        Some(scope) => {
            let scope = located(scope, lines);
            let value = match scope.value.as_str() {
                "none" => Scope::None,
                "own-subtree" => Scope::OwnSubtree,
                "own-project" => Scope::OwnProject,
                "projects" => Scope::Projects,
                other => {
                    return Err(ParseError {
                        line: scope.line,
                        message: format!(
                            "unknown graph_edits scope '{other}'; use none, own-subtree, \
                             own-project or projects"
                        ),
                    });
                }
            };
            Located {
                value,
                line: scope.line,
            }
        }
    };
    let land_at = match raw.land_at {
        None => LandAt::Step,
        Some(land_at) => {
            let land_at = located(land_at, lines);
            match land_at.value.as_str() {
                "step" => LandAt::Step,
                "run-end" => LandAt::RunEnd,
                other => {
                    return Err(ParseError {
                        line: land_at.line,
                        message: format!(
                            "unknown graph_edits land_at '{other}'; use step or run-end"
                        ),
                    });
                }
            }
        }
    };
    let projects_written = raw.projects.is_some();
    let projects = raw
        .projects
        .unwrap_or_default()
        .into_iter()
        .map(|project| located(project, lines))
        .collect::<Vec<_>>();
    if projects_written && scope.value != Scope::Projects {
        return Err(ParseError {
            line: projects.first().map_or(line, |project| project.line),
            message: format!(
                "graph_edits `projects` needs scope = \"projects\", not \"{}\"",
                scope.value.as_str()
            ),
        });
    }
    if scope.value == Scope::Projects && projects.is_empty() {
        return Err(ParseError {
            line: scope.line,
            message: "graph_edits scope = \"projects\" needs a non-empty `projects` list"
                .to_string(),
        });
    }
    Ok(GraphEditsDecl {
        line,
        scope,
        projects,
        land_at,
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

    let mut output: Vec<OutputField> = output.into_iter().map(|(_, field)| field).collect();
    let is_checks = raw.kind.get_ref() == &NodeKind::Program
        && raw
            .use_node
            .as_ref()
            .is_some_and(|use_node| use_node.get_ref() == CHECKS_PROGRAM);
    if is_checks && output.is_empty() {
        output = checks_output(line);
    }

    let schema = OutputSchema::from_fields(&output).map_err(|message| ParseError {
        line,
        message: format!("node '{name}': {message}"),
    })?;

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
        permissions: raw.permissions.map(|permissions| {
            let line = lines.line_of(&permissions);
            Permissions {
                line,
                ..permissions.into_inner()
            }
        }),
        output,
        output_schema: raw.output_schema.map(|file| located(file, lines)),
        schema: Arc::new(schema),
        routes,
        join: raw.join.map(|join| {
            let line = lines.line_of(&join);
            Join {
                line,
                ..join.into_inner()
            }
        }),
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
        settings: RawSettings {
            agent: raw.agent,
            model: raw.model,
            permission_mode: raw.permission_mode,
            allowed_tools: raw.allowed_tools,
            max_turns: raw.max_turns,
            max_cost_usd: raw.max_cost_usd,
            max_minutes: raw.max_minutes,
            retries: raw.retries,
        }
        .convert(lines),
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
        RawFieldType::GraphEdits => FieldType::GraphEdits,
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
