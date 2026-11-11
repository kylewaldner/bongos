//! The Linear API as the rest of bongos sees it: one trait, plain data types and one error type.
//! Nothing here carries a credential.

use std::fmt;

use futures::future::BoxFuture;
use uuid::Uuid;

use crate::ident::Identifier;

/// How a ticket is named in a request: by identifier (`SEQ-121`) or by UUID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IssueRef {
    Identifier(Identifier),
    Id(Uuid),
}

impl IssueRef {
    /// The string sent as the `id` argument of `issue(id:)`.
    pub fn as_arg(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for IssueRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IssueRef::Identifier(identifier) => identifier.fmt(f),
            IssueRef::Id(id) => write!(f, "{}", id.hyphenated()),
        }
    }
}

impl From<Identifier> for IssueRef {
    fn from(identifier: Identifier) -> Self {
        IssueRef::Identifier(identifier)
    }
}

impl From<Uuid> for IssueRef {
    fn from(id: Uuid) -> Self {
        IssueRef::Id(id)
    }
}

/// A workflow state's type. The schema declares it a `String`, so values this crate does not know
/// are kept in `Other` instead of failing the whole response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateKind {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
    /// The seventh documented value: the ticket duplicates another one.
    Duplicate,
    Other(String),
}

impl StateKind {
    pub fn parse(text: &str) -> Self {
        match text {
            "triage" => Self::Triage,
            "backlog" => Self::Backlog,
            "unstarted" => Self::Unstarted,
            "started" => Self::Started,
            "completed" => Self::Completed,
            "canceled" => Self::Canceled,
            "duplicate" => Self::Duplicate,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Triage => "triage",
            Self::Backlog => "backlog",
            Self::Unstarted => "unstarted",
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Canceled => "canceled",
            Self::Duplicate => "duplicate",
            Self::Other(other) => other,
        }
    }

    /// States bongos never moves a ticket into: canceled and duplicate are treated alike.
    pub fn is_dropped(&self) -> bool {
        matches!(self, Self::Canceled | Self::Duplicate)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowState {
    pub id: String,
    pub name: String,
    pub kind: StateKind,
    pub position: f64,
}

/// A ticket without its subtickets.
#[derive(Clone, Debug, PartialEq)]
pub struct IssueSummary {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub team_id: String,
    pub state: WorkflowState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    /// Empty when the ticket has no description.
    pub description: String,
    pub url: String,
    pub team_id: String,
    pub state: WorkflowState,
    pub parent: Option<IssueSummary>,
    /// One level of subtickets, at most `graphql::CHILDREN_CAP` of them.
    pub children: Vec<Issue>,
    /// True when the ticket has more subtickets than the cap.
    pub children_truncated: bool,
    /// Ids of the tickets this one blocks.
    pub blocks: Vec<String>,
}

impl Issue {
    pub fn summary(&self) -> IssueSummary {
        IssueSummary {
            id: self.id.clone(),
            identifier: self.identifier.clone(),
            title: self.title.clone(),
            url: self.url.clone(),
            team_id: self.team_id.clone(),
            state: self.state.clone(),
        }
    }
}

/// A ticket to create. The client picks the UUID, so a retry after an unknown outcome cannot
/// create a second ticket: Linear answers `Duplicate`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewIssue {
    pub id: Uuid,
    pub team_id: String,
    pub parent_id: Option<String>,
    pub title: String,
    /// Empty means no description.
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    #[error("Linear rejected the token")]
    Unauthorized,
    /// `reset_at` is Unix time in milliseconds, when known.
    #[error("Linear rate limit reached")]
    RateLimited { reset_at: Option<u64> },
    #[error("{0} not found in Linear")]
    NotFound(String),
    #[error("could not reach Linear: {0}")]
    Network(String),
    #[error("Linear error: {0}")]
    Server(String),
    /// A create used an id that already exists.
    #[error("{0} already exists in Linear")]
    Duplicate(String),
}

/// What bongos needs from Linear. Implemented over HTTP by the app and in memory by the fake.
pub trait LinearApi: Send + Sync {
    /// Reads tickets with their subtickets. References Linear does not know are left out of the
    /// result; callers match by `id` or `identifier`.
    fn issues(&self, ids: Vec<IssueRef>) -> BoxFuture<'static, Result<Vec<Issue>, ApiError>>;
    /// The tickets assigned to the viewer, most recently updated first.
    fn recent(&self, limit: usize) -> BoxFuture<'static, Result<Vec<IssueSummary>, ApiError>>;
    fn team_states(
        &self,
        team_id: &str,
    ) -> BoxFuture<'static, Result<Vec<WorkflowState>, ApiError>>;
    fn set_state(&self, issue_id: &str, state_id: &str)
    -> BoxFuture<'static, Result<(), ApiError>>;
    fn create(&self, input: NewIssue) -> BoxFuture<'static, Result<IssueSummary, ApiError>>;
    /// Checks the credentials and returns the workspace name.
    fn viewer(&self) -> BoxFuture<'static, Result<String, ApiError>>;
    /// `Some(reset_at)` when the last response said fewer than 20 requests remain (the reset time
    /// in Unix milliseconds, when it sent one); automatic refreshes then pause.
    fn low_requests(&self) -> Option<Option<u64>> {
        None
    }
}
