use serde::{Deserialize, Serialize};

/// A TOML datetime (`2026-11-08T10:00:00Z`). It reads and writes as a native TOML datetime, not a
/// string, so the files stay as the design shows them.
pub type Timestamp = toml::value::Datetime;

/// The `id` of a task (`task.toml`).
pub type TaskId = String;

/// The only schema version this crate reads and writes.
pub const SCHEMA: u32 = 1;

/// Keys this crate does not know. Kept on read and written back, so a later feature's fields
/// survive this one's writes.
pub type Extra = toml::Table;

fn schema_one() -> u32 {
    SCHEMA
}

/// A new task id: a UUID v7, which sorts by creation time.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The current time as a [`Timestamp`], in UTC with second precision.
pub fn now() -> Timestamp {
    let text = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    // An RFC 3339 string with a `Z` offset is always a valid TOML datetime.
    text.parse().unwrap_or_else(|_| unreachable!("{text}"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    PrShip,
    PrMerge,
    Document,
    Spike,
    Babysit,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Open,
    Done,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Visibility {
    #[default]
    Shared,
    HumanOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TodoRole {
    ProducesBranch,
    Ships,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TodoState {
    Open,
    Done,
    Skipped,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResultKind {
    Pr,
    Prs,
    Document,
    Link,
    Findings,
    Branch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResultState {
    Open,
    Approved,
    Merged,
    Closed,
}

/// How far a parent must get before a dependent task is unblocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnblockPoint {
    Review,
    Implemented,
    Merged,
}

/// An ordering edge, stored on the later sibling and naming the earlier one by slug.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub task: String,
    pub unblock: UnblockPoint,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The sibling this task's branch is stacked on, by slug.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StackedOn {
    pub task: String,
    pub unblock: UnblockPoint,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The marker left when the owner ships a task while its parent is not yet in the state the
/// edge asks for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShipOverridden {
    pub at: Timestamp,
    pub parent_state: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<TodoRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub state: TodoState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<Timestamp>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    pub kind: ResultKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ResultState>,
    /// The PRs of a `prs` result: the set a `babysit` task watches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: Timestamp,
    /// `kyle`, `workflow:<name>`, `agent` or `tree-run`.
    pub by: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `task.toml`. A group task has no `todos`, `result`, `branch` or `worktree`; those stay empty
/// and are not written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskFile {
    #[serde(default = "schema_one")]
    pub schema: u32,
    pub id: String,
    pub goal: String,
    pub kind: Kind,
    pub status: Status,
    pub created: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_is_ship: Option<bool>,
    #[serde(default)]
    pub default_visibility: Visibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebase_ack_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ship_overridden: Option<ShipOverridden>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<Edge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stacked_on: Option<StackedOn>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<Todo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<TaskResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<HistoryEntry>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl TaskFile {
    /// An open task with a new id, created now, and nothing else set.
    pub fn new(goal: impl Into<String>, kind: Kind) -> Self {
        Self {
            schema: SCHEMA,
            id: new_id(),
            goal: goal.into(),
            kind,
            status: Status::Open,
            created: now(),
            closed: None,
            repository: None,
            merge_is_ship: None,
            default_visibility: Visibility::Shared,
            activity: None,
            linear: None,
            branch: None,
            base: None,
            base_sha: None,
            rebase_ack_sha: None,
            worktree: None,
            ship_overridden: None,
            after: Vec::new(),
            stacked_on: None,
            todos: Vec::new(),
            result: None,
            history: Vec::new(),
            extra: Extra::new(),
        }
    }
}

/// `task.toml` keeps at most this many history entries; the oldest move to `history.old.toml`.
pub const HISTORY_LIMIT: usize = 200;

/// `history.old.toml`: the oldest history entries of a task, oldest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryFile {
    #[serde(default = "schema_one")]
    pub schema: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<HistoryEntry>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for HistoryFile {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            history: Vec::new(),
            extra: Extra::new(),
        }
    }
}

/// Where a project's documents live.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DocsHome {
    Git {
        path: String,
        #[serde(flatten)]
        extra: Extra,
    },
    Notion {
        page: String,
        #[serde(flatten)]
        extra: Extra,
    },
}

/// `project.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    #[serde(default = "schema_one")]
    pub schema: u32,
    pub title: String,
    #[serde(default)]
    pub repositories: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_home: Option<DocsHome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ProjectFile {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            schema: SCHEMA,
            title: title.into(),
            repositories: Vec::new(),
            docs_home: None,
            linear: None,
            archived: false,
            extra: Extra::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub path: String,
    #[serde(default)]
    pub description: String,
    /// Unset follows the task's `default_visibility`. Every entry of a merged list
    /// ([`crate::artifacts::merge`]) has it set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `artifacts.toml`: the descriptions and visibility of files in a task directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArtifactsFile {
    #[serde(default = "schema_one")]
    pub schema: u32,
    #[serde(default, rename = "artifact", skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Default for ArtifactsFile {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            artifacts: Vec::new(),
            extra: Extra::new(),
        }
    }
}

/// `<name>.link.toml`: a link to a document that lives elsewhere, such as a Notion page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkFile {
    #[serde(default = "schema_one")]
    pub schema: u32,
    pub url: String,
    pub title: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl LinkFile {
    pub fn new(url: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            schema: SCHEMA,
            url: url.into(),
            title: title.into(),
            extra: Extra::new(),
        }
    }
}
