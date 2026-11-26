//! The lane table: `lanes.toml` parsed with line numbers, and its validation.
//!
//! [`parse`] reads the text into a [`LaneTable`] and records the line of every kind, rule, pin and
//! field so [`validate_lane_table`] can name them. Unlike workflow definitions, an unknown key here
//! is an error. With no file at all a project has [`LaneTable::implicit`].

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use serde::Deserialize;
use toml::Spanned;

use crate::binding::check_tokens;
use crate::problem::Problem;

pub const SCHEMA_VERSION: u32 = 1;

const TOP_LEVEL_KEYS: &[&str] = &[
    "schema",
    "owner",
    "on_interruption",
    "dispatcher",
    "kind",
    "rules",
    "pin",
    "exempt",
    "locations",
];
const LOCATIONS_KEYS: &[&str] = &["worktree_root", "dirs_root"];
const KIND_KEYS: &[&str] = &[
    "name",
    "count",
    "ahead",
    "areas",
    "rules",
    "unattended",
    "binding",
];
const AHEAD_KEYS: &[&str] = &["cap"];
const BINDING_KEYS: &[&str] = &["env", "dirs", "trees", "floors"];
const TREE_KEYS: &[&str] = &["repo", "mode", "location", "base_tree"];
const BASE_TREE_KEYS: &[&str] = &["dirs"];
const RULE_KEYS: &[&str] = &[
    "work_kind",
    "state",
    "todo",
    "workflow",
    "use_todo_workflow",
    "inputs",
    "staleness",
    "refresh",
];
const PIN_KEYS: &[&str] = &["task", "lane", "kind"];
const EXEMPT_KEYS: &[&str] = &["tags", "tasks"];
/// Keys that a later feature will define; they are refused by name unless that feature
/// registered them (see [`parse_accepting`]). Their values are kept raw in [`RawFields`].
const NOT_AVAILABLE_KEYS: &[&str] = &["pools", "pulls", "hooks"];

/// A value with the line it was written on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located<T> {
    pub line: usize,
    pub value: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerMode {
    /// A leaf's owner is its top-level task (default).
    Top,
    /// Every leaf owns itself.
    SelfOwned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TreeMode {
    Persistent,
    #[default]
    Fresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneTable {
    pub schema: u32,
    pub owner: OwnerMode,
    pub on_interruption: Option<String>,
    pub dispatcher: Option<String>,
    pub kinds: Vec<LaneKind>,
    pub rules: Vec<DispatchRule>,
    pub pins: Vec<Pin>,
    pub exemptions: Vec<Exemption>,
    /// The project's own default locations (`[locations]`); empty when the table has none.
    pub locations: Locations,
    /// Problems found while parsing that still produced a table: unknown keys, bad counts.
    /// [`validate_lane_table`] reports them with its own.
    pub diagnostics: Vec<Diagnostic>,
}

/// The project's own default locations, `[locations]` in the lane table. They sit above the
/// user's worktree settings and below the project profile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Locations {
    /// `worktree_root = "<template>"`: the root of every repository's lane trees, a worktree root
    /// template with `{repo}` and `{slug}` as in the repository settings.
    pub worktree_root: Option<Located<String>>,
    /// `worktree_root = { <repo> = "<template>" }`: a root per repository.
    pub repository_roots: Vec<(String, Located<String>)>,
    /// `dirs_root = "<path>"`: named directories go to `<dirs_root>/<lane>/<name>`.
    pub dirs_root: Option<Located<String>>,
}

impl Locations {
    /// The worktree root template of `repo`: its own entry, else the shared one.
    pub fn worktree_root_of(&self, repo: &str) -> Option<&Located<String>> {
        self.repository_roots
            .iter()
            .find(|(name, _)| name == repo)
            .map(|(_, root)| root)
            .or(self.worktree_root.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneKind {
    pub name: String,
    pub line: usize,
    pub count: u32,
    pub count_line: usize,
    pub ahead: Option<Ahead>,
    pub areas: bool,
    pub rules: Vec<Located<String>>,
    pub unattended: Option<bool>,
    pub binding: Option<Binding>,
    /// `pools`, `pulls` and `hooks` written on the kind, unparsed.
    pub raw: RawFields,
}

/// The values of the keys another feature defines (`pools`, `pulls`, `hooks`), unparsed, by key,
/// in the order written. This crate keeps them; the feature that registered a key reads it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawFields(pub Vec<(String, Located<toml::Value>)>);

// A parsed TOML value compares equal to itself except for a NaN float, which no key kept here
// is meant to hold; tables holding these fields keep their `Eq`.
impl Eq for RawFields {}

impl RawFields {
    pub fn get(&self, key: &str) -> Option<&Located<toml::Value>> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn collect(lines: &LineIndex, fields: [(&str, Option<Spanned<toml::Value>>); 3]) -> Self {
        let mut kept: Vec<(usize, (String, Located<toml::Value>))> = fields
            .into_iter()
            .filter_map(|(key, value)| {
                let value = value?;
                Some((value.span().start, (key.to_string(), lines.located(value))))
            })
            .collect();
        kept.sort_by_key(|(start, _)| *start);
        Self(kept.into_iter().map(|(_, entry)| entry).collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ahead {
    pub line: usize,
    pub cap: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Binding {
    pub line: usize,
    pub env: Vec<(String, Located<String>)>,
    pub dirs: Vec<Located<String>>,
    pub trees: Vec<TreeSpec>,
    pub floors: Vec<Located<String>>,
    /// `pools`, `pulls` and `hooks` written in the binding, unparsed.
    pub raw: RawFields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSpec {
    pub line: usize,
    pub repo: String,
    pub mode: TreeMode,
    pub location: Option<String>,
    pub base_tree: Option<BaseTree>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseTree {
    pub line: usize,
    pub dirs: Vec<Located<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchRule {
    pub name: String,
    pub line: usize,
    pub work_kind: Option<String>,
    pub state: Vec<String>,
    pub todo: Option<String>,
    pub workflow: Option<Located<String>>,
    pub use_todo_workflow: bool,
    pub inputs: Vec<(String, Located<String>)>,
    pub staleness: Option<Located<String>>,
    pub refresh: Option<Located<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub line: usize,
    pub task: Option<Located<String>>,
    pub lane: Option<Located<String>>,
    pub kind: Option<Located<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exemption {
    pub line: usize,
    pub tags: Vec<Located<String>>,
    pub tasks: Vec<Located<String>>,
}

impl LaneTable {
    /// The table of a project with no `lanes.toml`: one kind `default`, count 1, no rules, areas
    /// on, no binding. A file never yields this implicitly.
    pub fn implicit() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            owner: OwnerMode::Top,
            on_interruption: None,
            dispatcher: None,
            kinds: vec![LaneKind {
                name: "default".to_string(),
                line: 0,
                count: 1,
                count_line: 0,
                ahead: None,
                areas: true,
                rules: Vec::new(),
                unattended: None,
                binding: None,
                raw: RawFields::default(),
            }],
            rules: Vec::new(),
            pins: Vec::new(),
            exemptions: Vec::new(),
            locations: Locations::default(),
            diagnostics: Vec::new(),
        }
    }

    pub fn kind_names(&self) -> Vec<&str> {
        self.kinds.iter().map(|kind| kind.name.as_str()).collect()
    }
}

/// What validation resolves names against.
#[derive(Debug, Clone, Default)]
pub struct ValidationContext {
    /// Workflow names that resolve in the project's layers.
    pub workflow_names: BTreeSet<String>,
    /// Ids of the tasks in master.
    pub task_ids: BTreeSet<String>,
}

#[derive(Deserialize)]
struct RawTable {
    schema: Spanned<u32>,
    owner: Option<Spanned<String>>,
    on_interruption: Option<String>,
    dispatcher: Option<String>,
    #[serde(default)]
    kind: Vec<Spanned<RawKind>>,
    #[serde(default)]
    rules: HashMap<String, Spanned<RawRule>>,
    #[serde(default)]
    pin: Vec<Spanned<RawPin>>,
    #[serde(default)]
    exempt: Vec<Spanned<RawExempt>>,
    locations: Option<RawLocations>,
}

#[derive(Deserialize)]
struct RawLocations {
    worktree_root: Option<RawRoot>,
    dirs_root: Option<Spanned<String>>,
}

/// `worktree_root`: one template for every repository, or one per repository.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawRoot {
    Shared(String),
    PerRepository(std::collections::BTreeMap<String, String>),
}

#[derive(Deserialize)]
struct RawKind {
    name: Spanned<String>,
    count: Option<Spanned<toml::Value>>,
    ahead: Option<Spanned<RawAhead>>,
    areas: Option<bool>,
    #[serde(default)]
    rules: Vec<Spanned<String>>,
    unattended: Option<bool>,
    binding: Option<Spanned<RawBinding>>,
    pools: Option<Spanned<toml::Value>>,
    pulls: Option<Spanned<toml::Value>>,
    hooks: Option<Spanned<toml::Value>>,
}

#[derive(Deserialize)]
struct RawAhead {
    cap: Option<Spanned<toml::Value>>,
}

#[derive(Deserialize)]
struct RawBinding {
    #[serde(default)]
    env: HashMap<String, Spanned<String>>,
    #[serde(default)]
    dirs: Vec<Spanned<String>>,
    #[serde(default)]
    trees: Vec<Spanned<RawTree>>,
    #[serde(default)]
    floors: Vec<Spanned<String>>,
    pools: Option<Spanned<toml::Value>>,
    pulls: Option<Spanned<toml::Value>>,
    hooks: Option<Spanned<toml::Value>>,
}

#[derive(Deserialize)]
struct RawTree {
    repo: String,
    mode: Option<Spanned<String>>,
    location: Option<String>,
    base_tree: Option<Spanned<RawBaseTree>>,
}

#[derive(Deserialize)]
struct RawBaseTree {
    #[serde(default)]
    dirs: Vec<Spanned<String>>,
}

#[derive(Deserialize)]
struct RawRule {
    work_kind: Option<String>,
    #[serde(default)]
    state: Vec<String>,
    todo: Option<String>,
    workflow: Option<Spanned<String>>,
    #[serde(default)]
    use_todo_workflow: bool,
    #[serde(default)]
    inputs: HashMap<String, Spanned<String>>,
    staleness: Option<Spanned<String>>,
    refresh: Option<Spanned<String>>,
}

#[derive(Deserialize)]
struct RawPin {
    task: Option<Spanned<String>>,
    lane: Option<Spanned<String>>,
    kind: Option<Spanned<String>>,
}

#[derive(Deserialize)]
struct RawExempt {
    #[serde(default)]
    tags: Vec<Spanned<String>>,
    #[serde(default)]
    tasks: Vec<Spanned<String>>,
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

    fn located<T>(&self, spanned: Spanned<T>) -> Located<T> {
        Located {
            line: self.line_of(&spanned),
            value: spanned.into_inner(),
        }
    }

    fn error(&self, span: Option<Range<usize>>, message: impl Into<String>) -> ParseError {
        ParseError {
            line: span.map_or(1, |span| self.line(span.start)),
            message: message.into(),
        }
    }
}

/// Parses `lanes.toml`. Syntax and type errors name their line; unknown keys and bad counts are
/// kept in [`LaneTable::diagnostics`] for [`validate_lane_table`].
pub fn parse(text: &str) -> Result<LaneTable, ParseError> {
    parse_accepting(text, &[])
}

/// [`parse`], except that the keys another feature registered are not refused. A key is named by
/// its path: `kind.pulls`, `kind.binding.pools`, `kind.binding.hooks`. Their values are kept in
/// [`LaneKind::raw`] and [`Binding::raw`] either way.
pub fn parse_accepting(text: &str, accepted: &[&str]) -> Result<LaneTable, ParseError> {
    let lines = LineIndex::new(text);
    let raw: RawTable =
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

    let mut diagnostics = Vec::new();
    collect_unknown_keys(text, &lines, accepted, &mut diagnostics)?;

    let owner = match raw.owner {
        None => OwnerMode::Top,
        Some(owner) => match owner.get_ref().as_str() {
            "top" => OwnerMode::Top,
            "self" => OwnerMode::SelfOwned,
            other => {
                diagnostics.push(Diagnostic {
                    line: lines.line_of(&owner),
                    message: format!("owner must be \"top\" or \"self\", not \"{other}\""),
                });
                OwnerMode::Top
            }
        },
    };

    let kinds = raw
        .kind
        .into_iter()
        .map(|kind| convert_kind(kind, &lines, &mut diagnostics))
        .collect();

    let mut rules = raw
        .rules
        .into_iter()
        .map(|(name, rule)| (rule.span().start, convert_rule(name, rule, &lines)))
        .collect::<Vec<_>>();
    rules.sort_by_key(|(start, _)| *start);

    let pins = raw
        .pin
        .into_iter()
        .map(|pin| Pin {
            line: lines.line_of(&pin),
            task: pin.get_ref().task.clone().map(|task| lines.located(task)),
            lane: pin.get_ref().lane.clone().map(|lane| lines.located(lane)),
            kind: pin.get_ref().kind.clone().map(|kind| lines.located(kind)),
        })
        .collect();

    let exemptions = raw
        .exempt
        .into_iter()
        .map(|exempt| Exemption {
            line: lines.line_of(&exempt),
            tags: exempt
                .get_ref()
                .tags
                .iter()
                .cloned()
                .map(|tag| lines.located(tag))
                .collect(),
            tasks: exempt
                .get_ref()
                .tasks
                .iter()
                .cloned()
                .map(|task| lines.located(task))
                .collect(),
        })
        .collect();

    let locations = raw
        .locations
        .map(|locations| convert_locations(locations, text, &lines))
        .unwrap_or_default();

    Ok(LaneTable {
        schema: *raw.schema.get_ref(),
        owner,
        on_interruption: raw.on_interruption,
        dispatcher: raw.dispatcher,
        kinds,
        rules: rules.into_iter().map(|(_, rule)| rule).collect(),
        pins,
        exemptions,
        locations,
        diagnostics,
    })
}

fn convert_locations(raw: RawLocations, text: &str, lines: &LineIndex) -> Locations {
    let mut locations = Locations {
        dirs_root: raw.dirs_root.map(|root| lines.located(root)),
        ..Locations::default()
    };
    // The line of a key under `locations.worktree_root`, read with toml_edit: serde's spans do
    // not reach into a value that is either a string or a table.
    let document = toml_edit::ImDocument::parse(text).ok();
    let root_item = document
        .as_ref()
        .and_then(|document| document.as_table().get("locations"))
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|table| table.get_key_value("worktree_root"));
    let line_of = |key: Option<&toml_edit::Key>| {
        key.and_then(toml_edit::Key::span)
            .map_or(1, |span| lines.line(span.start))
    };
    match raw.worktree_root {
        None => {}
        Some(RawRoot::Shared(value)) => {
            let line = line_of(root_item.map(|(key, _)| key));
            locations.worktree_root = Some(Located { line, value });
        }
        Some(RawRoot::PerRepository(roots)) => {
            let table = root_item.and_then(|(_, item)| item.as_table_like());
            locations.repository_roots = roots
                .into_iter()
                .map(|(repo, value)| {
                    let key = table
                        .and_then(|table| table.get_key_value(&repo))
                        .map(|(key, _)| key);
                    let line = line_of(key);
                    (repo, Located { line, value })
                })
                .collect();
        }
    }
    locations
}

fn non_negative(
    value: &Spanned<toml::Value>,
    what: &str,
    lines: &LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<u32> {
    let parsed = value
        .get_ref()
        .as_integer()
        .and_then(|number| u32::try_from(number).ok());
    if parsed.is_none() {
        diagnostics.push(Diagnostic {
            line: lines.line_of(value),
            message: format!("{what} must be a non-negative integer"),
        });
    }
    parsed
}

fn convert_kind(
    kind: Spanned<RawKind>,
    lines: &LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) -> LaneKind {
    let line = lines.line_of(&kind);
    let raw = kind.into_inner();
    let name = raw.name.into_inner();
    if !is_kind_name(&name) {
        diagnostics.push(Diagnostic {
            line,
            message: format!("kind name \"{name}\" must match [a-z][a-z0-9]*"),
        });
    }
    let (count, count_line) = match &raw.count {
        None => (1, line),
        Some(value) => (
            non_negative(value, "count", lines, diagnostics).unwrap_or(1),
            lines.line_of(value),
        ),
    };
    let ahead = raw.ahead.map(|ahead| {
        let ahead_line = lines.line_of(&ahead);
        let cap = match &ahead.get_ref().cap {
            None => 1,
            Some(value) => non_negative(value, "ahead.cap", lines, diagnostics).unwrap_or(1),
        };
        Ahead {
            line: ahead_line,
            cap,
        }
    });
    let binding = raw
        .binding
        .map(|binding| convert_binding(binding, lines, diagnostics));
    let raw_fields = RawFields::collect(
        lines,
        [
            ("pools", raw.pools),
            ("pulls", raw.pulls),
            ("hooks", raw.hooks),
        ],
    );
    LaneKind {
        name,
        line,
        count,
        count_line,
        ahead,
        areas: raw.areas.unwrap_or(true),
        rules: raw
            .rules
            .into_iter()
            .map(|rule| lines.located(rule))
            .collect(),
        unattended: raw.unattended,
        binding,
        raw: raw_fields,
    }
}

fn convert_binding(
    binding: Spanned<RawBinding>,
    lines: &LineIndex,
    diagnostics: &mut Vec<Diagnostic>,
) -> Binding {
    let line = lines.line_of(&binding);
    let raw = binding.into_inner();
    let mut env = raw
        .env
        .into_iter()
        .map(|(name, value)| (value.span().start, (name, lines.located(value))))
        .collect::<Vec<_>>();
    env.sort_by_key(|(start, _)| *start);
    let trees = raw
        .trees
        .into_iter()
        .map(|tree| {
            let tree_line = lines.line_of(&tree);
            let raw = tree.into_inner();
            let mode = match raw.mode {
                None => TreeMode::Fresh,
                Some(mode) => match mode.get_ref().as_str() {
                    "persistent" => TreeMode::Persistent,
                    "fresh" => TreeMode::Fresh,
                    other => {
                        diagnostics.push(Diagnostic {
                            line: lines.line_of(&mode),
                            message: format!(
                                "tree mode must be \"persistent\" or \"fresh\", not \"{other}\""
                            ),
                        });
                        TreeMode::Fresh
                    }
                },
            };
            TreeSpec {
                line: tree_line,
                repo: raw.repo,
                mode,
                location: raw.location,
                base_tree: raw.base_tree.map(|base| BaseTree {
                    line: lines.line_of(&base),
                    dirs: base
                        .into_inner()
                        .dirs
                        .into_iter()
                        .map(|dir| lines.located(dir))
                        .collect(),
                }),
            }
        })
        .collect();
    Binding {
        line,
        env: env.into_iter().map(|(_, entry)| entry).collect(),
        dirs: raw.dirs.into_iter().map(|dir| lines.located(dir)).collect(),
        trees,
        floors: raw
            .floors
            .into_iter()
            .map(|floor| lines.located(floor))
            .collect(),
        raw: RawFields::collect(
            lines,
            [
                ("pools", raw.pools),
                ("pulls", raw.pulls),
                ("hooks", raw.hooks),
            ],
        ),
    }
}

fn convert_rule(name: String, rule: Spanned<RawRule>, lines: &LineIndex) -> DispatchRule {
    let line = lines.line_of(&rule);
    let raw = rule.into_inner();
    let mut inputs = raw
        .inputs
        .into_iter()
        .map(|(name, value)| (value.span().start, (name, lines.located(value))))
        .collect::<Vec<_>>();
    inputs.sort_by_key(|(start, _)| *start);
    DispatchRule {
        name,
        line,
        work_kind: raw.work_kind,
        state: raw.state,
        todo: raw.todo,
        workflow: raw.workflow.map(|value| lines.located(value)),
        use_todo_workflow: raw.use_todo_workflow,
        inputs: inputs.into_iter().map(|(_, entry)| entry).collect(),
        staleness: raw.staleness.map(|value| lines.located(value)),
        refresh: raw.refresh.map(|value| lines.located(value)),
    }
}

fn is_kind_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn tables_in(item: &toml_edit::Item) -> Vec<&dyn toml_edit::TableLike> {
    match item {
        toml_edit::Item::Table(table) => vec![table as &dyn toml_edit::TableLike],
        toml_edit::Item::ArrayOfTables(tables) => tables
            .iter()
            .map(|table| table as &dyn toml_edit::TableLike)
            .collect(),
        toml_edit::Item::Value(toml_edit::Value::InlineTable(table)) => {
            vec![table as &dyn toml_edit::TableLike]
        }
        toml_edit::Item::Value(toml_edit::Value::Array(array)) => array
            .iter()
            .filter_map(toml_edit::Value::as_inline_table)
            .map(|table| table as &dyn toml_edit::TableLike)
            .collect(),
        _ => Vec::new(),
    }
}

fn children<'a>(
    table: &'a dyn toml_edit::TableLike,
    key: &str,
) -> Vec<&'a dyn toml_edit::TableLike> {
    table.get(key).map(tables_in).unwrap_or_default()
}

/// Where a table sits, for the keys other features register: its key path (`kind`,
/// `kind.binding`) and whether an unregistered key of [`NOT_AVAILABLE_KEYS`] is refused by name.
#[derive(Clone, Copy)]
struct Seam<'a> {
    path: Option<&'a str>,
    names_not_available: bool,
    accepted: &'a [&'a str],
}

impl Seam<'_> {
    fn accepts(&self, key: &str) -> bool {
        self.path.is_some_and(|path| {
            let named = format!("{path}.{key}");
            NOT_AVAILABLE_KEYS.contains(&key)
                && self.accepted.iter().any(|accepted| *accepted == named)
        })
    }
}

fn collect_unknown_keys(
    text: &str,
    lines: &LineIndex,
    accepted: &[&str],
    out: &mut Vec<Diagnostic>,
) -> Result<(), ParseError> {
    let document = toml_edit::ImDocument::parse(text)
        .map_err(|error| lines.error(error.span(), error.message()))?;
    let root = document.as_table();
    let plain = Seam {
        path: None,
        names_not_available: false,
        accepted,
    };

    check_keys(
        root,
        TOP_LEVEL_KEYS,
        "the lane table",
        Seam {
            names_not_available: true,
            ..plain
        },
        lines,
        out,
    );

    for kind in children(root, "kind") {
        let kind_seam = Seam {
            path: Some("kind"),
            names_not_available: true,
            accepted,
        };
        check_keys(kind, KIND_KEYS, "[[kind]]", kind_seam, lines, out);
        for ahead in children(kind, "ahead") {
            check_keys(ahead, AHEAD_KEYS, "ahead", plain, lines, out);
        }
        for binding in children(kind, "binding") {
            let binding_seam = Seam {
                path: Some("kind.binding"),
                names_not_available: false,
                accepted,
            };
            check_keys(
                binding,
                BINDING_KEYS,
                "[kind.binding]",
                binding_seam,
                lines,
                out,
            );
            for tree in children(binding, "trees") {
                check_keys(tree, TREE_KEYS, "a tree", plain, lines, out);
                for base in children(tree, "base_tree") {
                    check_keys(base, BASE_TREE_KEYS, "base_tree", plain, lines, out);
                }
            }
        }
    }
    if let Some(rules) = root.get("rules").and_then(toml_edit::Item::as_table_like) {
        for (_, item) in rules.iter() {
            for rule in tables_in(item) {
                check_keys(rule, RULE_KEYS, "a rule", plain, lines, out);
            }
        }
    }
    for pin in children(root, "pin") {
        check_keys(pin, PIN_KEYS, "[[pin]]", plain, lines, out);
    }
    for exempt in children(root, "exempt") {
        check_keys(exempt, EXEMPT_KEYS, "[[exempt]]", plain, lines, out);
    }
    for locations in children(root, "locations") {
        check_keys(locations, LOCATIONS_KEYS, "[locations]", plain, lines, out);
    }
    Ok(())
}

fn check_keys(
    table: &dyn toml_edit::TableLike,
    known: &[&str],
    place: &str,
    seam: Seam,
    lines: &LineIndex,
    out: &mut Vec<Diagnostic>,
) {
    for (key, item) in table.iter() {
        if known.contains(&key) || seam.accepts(key) {
            continue;
        }
        let span = table
            .get_key_value(key)
            .and_then(|(key, _)| key.span())
            .or_else(|| item.span());
        let line = span.map_or(1, |span| lines.line(span.start));
        let message = if seam.names_not_available && NOT_AVAILABLE_KEYS.contains(&key) {
            format!("{key} are not available yet")
        } else {
            format!("unknown key \"{key}\" in {place}")
        };
        out.push(Diagnostic { line, message });
    }
}

/// Returns the lane instance (`<kind>-<digits>` for a declared kind) that `text` names, if any.
/// Words are runs of letters, digits, `_` and `-`, so `rebuild-2`, `pre-build-2` and `build-x`
/// do not name an instance of kind `build`.
pub fn names_lane_instance(text: &str, kinds: &[&str]) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .find_map(|word| {
            let (kind, digits) = word.rsplit_once('-')?;
            let is_instance = kinds.contains(&kind)
                && !digits.is_empty()
                && digits.bytes().all(|byte| byte.is_ascii_digit());
            is_instance.then(|| word.to_string())
        })
}

/// Checks the text of one prompt or script (`file` is its name for the message) for lane
/// instances. Every hit is a problem naming the line.
pub fn prompt_lane_problems(file: &str, text: &str, kinds: &[&str]) -> Vec<Problem> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let lane = names_lane_instance(line, kinds)?;
            Some(Problem {
                requirement: "dispatch rules and prompts name lane kinds, never lane instances"
                    .to_string(),
                message: format!(
                    "prompt {file} line {} names lane {lane}; name the kind",
                    index + 1
                ),
                hint: Some(
                    "write the kind, for example the kind name without its number".to_string(),
                ),
            })
        })
        .collect()
}

/// Every validation of the lane table; each diagnostic names its line.
pub fn validate_lane_table(table: &LaneTable, ctx: &ValidationContext) -> Vec<Diagnostic> {
    let mut out = table.diagnostics.clone();
    let kinds = table.kind_names();
    let mut push = |line: usize, message: String| out.push(Diagnostic { line, message });

    let mut first_seen: HashMap<&str, usize> = HashMap::new();
    for kind in &table.kinds {
        match first_seen.get(kind.name.as_str()) {
            Some(first) => push(
                kind.line,
                format!(
                    "kind \"{}\" is declared twice (first on line {first})",
                    kind.name
                ),
            ),
            None => {
                first_seen.insert(&kind.name, kind.line);
            }
        }
        for rule in &kind.rules {
            if !table
                .rules
                .iter()
                .any(|declared| declared.name == rule.value)
            {
                push(
                    rule.line,
                    format!(
                        "kind \"{}\" lists rule \"{}\", which is not declared",
                        kind.name, rule.value
                    ),
                );
            }
        }
        if let Some(binding) = &kind.binding {
            validate_binding(&kind.name, binding, &kinds, &mut push);
        }
    }

    for rule in &table.rules {
        validate_rule(rule, &kinds, ctx, &mut push);
    }

    for pin in &table.pins {
        validate_pin(pin, &kinds, ctx, &mut push);
    }

    for exempt in &table.exemptions {
        if exempt.tags.is_empty() && exempt.tasks.is_empty() {
            push(exempt.line, "an exemption needs tags or tasks".to_string());
        }
        for task in &exempt.tasks {
            if !ctx.task_ids.contains(&task.value) {
                push(
                    task.line,
                    format!(
                        "exemption names task {}, which is not in master",
                        task.value
                    ),
                );
            }
        }
    }

    out.sort_by_key(|diagnostic| diagnostic.line);
    out
}

fn validate_binding(
    kind: &str,
    binding: &Binding,
    kinds: &[&str],
    push: &mut impl FnMut(usize, String),
) {
    let dir_names: Vec<&str> = binding.dirs.iter().map(|dir| dir.value.as_str()).collect();
    for (name, value) in &binding.env {
        if let Err(error) = check_tokens(&value.value, &dir_names) {
            push(value.line, format!("env {name}: {error}"));
        }
        if let Some(lane) = names_lane_instance(&value.value, kinds) {
            push(
                value.line,
                format!("env {name} names lane {lane}; name the kind"),
            );
        }
    }
    for tree in &binding.trees {
        let Some(base) = &tree.base_tree else {
            continue;
        };
        for dir in &base.dirs {
            if !dir_names.contains(&dir.value.as_str()) {
                push(
                    dir.line,
                    format!(
                        "base_tree dirs names \"{}\", which kind \"{kind}\" does not bind in dirs",
                        dir.value
                    ),
                );
            }
        }
    }
}

fn validate_rule(
    rule: &DispatchRule,
    kinds: &[&str],
    ctx: &ValidationContext,
    push: &mut impl FnMut(usize, String),
) {
    for (field, located) in [("workflow", &rule.workflow), ("refresh", &rule.refresh)] {
        if let Some(located) = located
            && !ctx.workflow_names.contains(&located.value)
        {
            push(
                located.line,
                format!(
                    "rule \"{}\" {field} \"{}\" does not resolve to a workflow",
                    rule.name, located.value
                ),
            );
        }
    }
    if let Some(staleness) = &rule.staleness
        && rule.refresh.is_none()
    {
        push(
            staleness.line,
            format!("rule \"{}\" sets staleness without refresh", rule.name),
        );
    }

    let mut texts: Vec<(usize, &str)> = Vec::new();
    texts.extend(rule.work_kind.iter().map(|text| (rule.line, text.as_str())));
    texts.extend(rule.todo.iter().map(|text| (rule.line, text.as_str())));
    for located in [&rule.workflow, &rule.staleness, &rule.refresh]
        .into_iter()
        .flatten()
    {
        texts.push((located.line, located.value.as_str()));
    }
    texts.extend(
        rule.inputs
            .iter()
            .map(|(_, input)| (input.line, input.value.as_str())),
    );
    for (line, text) in texts {
        if let Some(lane) = names_lane_instance(text, kinds) {
            push(
                line,
                format!("rule \"{}\" names lane {lane}; name the kind", rule.name),
            );
        }
    }
}

fn validate_pin(
    pin: &Pin,
    kinds: &[&str],
    ctx: &ValidationContext,
    push: &mut impl FnMut(usize, String),
) {
    match &pin.task {
        None => push(pin.line, "a pin needs a task".to_string()),
        Some(task) if !ctx.task_ids.contains(&task.value) => push(
            task.line,
            format!("pin names task {}, which is not in master", task.value),
        ),
        Some(_) => {}
    }
    match (&pin.lane, &pin.kind) {
        (None, None) => push(pin.line, "a pin needs a lane or a kind".to_string()),
        (Some(lane), Some(_)) => push(
            lane.line,
            "a pin names a lane or a kind, not both".to_string(),
        ),
        (Some(lane), None) => {
            let parsed = lane.value.rsplit_once('-').filter(|(_, digits)| {
                digits.parse::<u32>().is_ok_and(|index| index >= 1)
                    && digits.bytes().all(|byte| byte.is_ascii_digit())
            });
            match parsed {
                None => push(
                    lane.line,
                    format!("pin lane \"{}\" is not of the form <kind>-<n>", lane.value),
                ),
                Some((kind, _)) if !kinds.contains(&kind) => push(
                    lane.line,
                    format!(
                        "pin names lane \"{}\" but kind \"{kind}\" does not exist",
                        lane.value
                    ),
                ),
                Some(_) => {}
            }
        }
        (None, Some(kind)) => {
            if !kinds.contains(&kind.value.as_str()) {
                push(
                    kind.line,
                    format!("pin names kind \"{}\", which does not exist", kind.value),
                );
            }
        }
    }
}
