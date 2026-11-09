//! The syntax tree of a proposal file. It holds labels, goals and ids as written, with the 1-based
//! line of every element, so later validation can point at the line. Nothing here is checked
//! against a task tree.

use bongos_work_model::rules::EdgeKind;
use bongos_work_model::{Kind, TodoRole, UnblockPoint};

/// The only format version this crate reads and writes: `format 1`.
pub const FORMAT_VERSION: u32 = 1;

/// What a label may be, for messages.
pub const LABEL_HELP: &str =
    "use lowercase letters, digits and '-', starting with a letter or digit";

pub const KIND_NAMES: [(&str, Kind); 6] = [
    ("pr-ship", Kind::PrShip),
    ("pr-merge", Kind::PrMerge),
    ("document", Kind::Document),
    ("spike", Kind::Spike),
    ("babysit", Kind::Babysit),
    ("other", Kind::Other),
];

pub const ROLE_NAMES: [(&str, TodoRole); 3] = [
    ("produces-branch", TodoRole::ProducesBranch),
    ("ships", TodoRole::Ships),
    ("plans", TodoRole::Plans),
];

pub const UNBLOCK_NAMES: [(&str, UnblockPoint); 3] = [
    ("implemented", UnblockPoint::Implemented),
    ("review", UnblockPoint::Review),
    ("merged", UnblockPoint::Merged),
];

pub fn kind_name(kind: Kind) -> &'static str {
    KIND_NAMES
        .iter()
        .find(|(_, candidate)| *candidate == kind)
        .map_or("other", |(name, _)| name)
}

pub fn unblock_name(point: UnblockPoint) -> &'static str {
    UNBLOCK_NAMES
        .iter()
        .find(|(_, candidate)| *candidate == point)
        .map_or("merged", |(name, _)| name)
}

/// A value with the 1-based line it was written on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located<T> {
    pub value: T,
    pub line: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// One problem in a proposal file. The parser reports only errors; validation adds warnings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// 1-based.
    pub line: usize,
    pub severity: Severity,
    pub message: String,
}

impl Problem {
    pub fn error(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            severity: Severity::Error,
            message: message.into(),
        }
    }
}

/// The `by` header line: who wrote the proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum By {
    Kyle,
    Chat(String),
    Workflow(String),
}

/// The header lines, each present only when written. They go in the order `format`, `proposal`,
/// `for`, `by`, `why`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// The `format N` line, which is optional and only read for its version.
    pub format: Option<Located<u32>>,
    pub number: Option<Located<u32>>,
    /// The id of the task whose directory holds the file.
    pub for_task: Option<Located<String>>,
    pub by: Option<Located<By>>,
    /// The text after `why`, empty for a bare `why`.
    pub why: Option<Located<String>>,
}

/// The attributes after ` | ` on a task line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskAttributes {
    pub id: Option<String>,
    pub kind: Option<Kind>,
    pub keeps_work: bool,
    pub drop: bool,
    /// Printed for reading; never an instruction.
    pub state: Option<String>,
}

/// The `done`, `skipped` or `cancelled` mark on a todo line. Printed for reading; never an
/// instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodoMark {
    Done,
    Skipped,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub line: usize,
    pub name: String,
    pub role: Option<TodoRole>,
    pub workflow: Option<String>,
    pub mark: Option<TodoMark>,
    pub started: bool,
}

/// An `after` or `stacked on` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub line: usize,
    pub kind: EdgeKind,
    /// The target as labels: one label for a sibling, more for a path that names a task on
    /// another level (`billing/schema`), which validation refuses. Never empty.
    pub target: Vec<String>,
    pub unblock: Option<UnblockPoint>,
}

/// One task line and everything under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub line: usize,
    pub label: String,
    pub goal: String,
    pub attributes: TaskAttributes,
    /// Every `result` line, in file order. A leaf has one; a second is for validation to refuse.
    pub results: Vec<Located<String>>,
    pub todos: Vec<Todo>,
    pub edges: Vec<Edge>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn new(line: usize, label: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            line,
            label: label.into(),
            goal: goal.into(),
            attributes: TaskAttributes::default(),
            results: Vec::new(),
            todos: Vec::new(),
            edges: Vec::new(),
            children: Vec::new(),
        }
    }
}

/// The result of parsing a proposal file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub header: Header,
    /// The one top-level task. When the file has none, an empty node on line 0 with an empty
    /// label, and a problem says so.
    pub root: Node,
    /// In line order.
    pub problems: Vec<Problem>,
}

impl Parsed {
    pub fn has_errors(&self) -> bool {
        self.problems
            .iter()
            .any(|problem| problem.severity == Severity::Error)
    }
}

/// Whether `text` is a valid label: `[a-z0-9][a-z0-9-]*`.
pub fn is_label(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && characters.all(|next| next.is_ascii_lowercase() || next.is_ascii_digit() || next == '-')
}
