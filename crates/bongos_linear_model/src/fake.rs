//! An in-memory `LinearApi` for tests. It records every call (operation and ids only), can fail
//! the next call with any `ApiError`, and pages subtickets like the real API: a first page, then
//! `after:` cursors, up to the same cap.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use futures::FutureExt as _;
use futures::future::BoxFuture;
use uuid::Uuid;

use crate::api::{
    ApiError, Issue, IssueRef, IssueSummary, LinearApi, NewIssue, StateKind, WorkflowState,
};
use crate::graphql::{
    CHILDREN_CAP, CHILDREN_FIRST_PAGE, ChildrenPage, apply_children_page, next_children_first,
};
use crate::ident::{Identifier, issue_url};

/// One call made to the fake. Never holds a credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Issues(Vec<String>),
    Recent(usize),
    TeamStates(String),
    SetState {
        issue_id: String,
        state_id: String,
    },
    Create {
        id: Uuid,
        team_id: String,
        parent_id: Option<String>,
        title: String,
    },
    Viewer,
}

struct Team {
    key: String,
    states: Vec<WorkflowState>,
}

struct Inner {
    workspace: String,
    teams: BTreeMap<String, Team>,
    /// Flat, in creation order; children are the issues whose `parent` is the issue.
    issues: Vec<Issue>,
    calls: Vec<Call>,
    fail_next: Option<ApiError>,
    page_size: Option<usize>,
}

#[derive(Clone)]
pub struct FakeLinear {
    inner: Arc<Mutex<Inner>>,
}

impl Default for FakeLinear {
    fn default() -> Self {
        Self::new("acme")
    }
}

impl FakeLinear {
    pub fn new(workspace: &str) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                workspace: workspace.to_string(),
                teams: BTreeMap::new(),
                issues: Vec::new(),
                calls: Vec::new(),
                fail_next: None,
                page_size: None,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add_team(&self, team_id: &str, key: &str, states: Vec<WorkflowState>) {
        self.lock().teams.insert(
            team_id.to_string(),
            Team {
                key: key.to_string(),
                states,
            },
        );
    }

    /// Adds a ticket. Its `children` are ignored: subtickets are other tickets whose `parent`
    /// is this one.
    pub fn add_issue(&self, mut issue: Issue) {
        issue.children = Vec::new();
        issue.children_truncated = false;
        self.lock().issues.push(issue);
    }

    /// Makes the next call return `error` (it is still recorded).
    pub fn fail_next(&self, error: ApiError) {
        self.lock().fail_next = Some(error);
    }

    /// Overrides the page size used for subtickets (first page and follow-ups).
    pub fn set_page_size(&self, size: usize) {
        self.lock().page_size = Some(size.max(1));
    }

    pub fn calls(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    pub fn clear_calls(&self) {
        self.lock().calls.clear();
    }

    /// The stored ticket (without subtickets), by id or identifier.
    pub fn issue(&self, id_or_identifier: &str) -> Option<Issue> {
        self.lock()
            .issues
            .iter()
            .find(|i| i.id == id_or_identifier || i.identifier == id_or_identifier)
            .cloned()
    }

    /// One page of a ticket's subtickets, like `children(first:, after:)`. The cursor is the
    /// index of the last child returned, as text.
    pub fn children_page(
        &self,
        parent_id: &str,
        first: usize,
        after: Option<&str>,
    ) -> ChildrenPage {
        children_page(&self.lock(), parent_id, first, after)
    }

    fn begin(&self, call: Call) -> Result<MutexGuard<'_, Inner>, ApiError> {
        let mut inner = self.lock();
        inner.calls.push(call);
        match inner.fail_next.take() {
            Some(error) => Err(error),
            None => Ok(inner),
        }
    }
}

fn children_page(
    inner: &Inner,
    parent_id: &str,
    first: usize,
    after: Option<&str>,
) -> ChildrenPage {
    let all: Vec<&Issue> = inner
        .issues
        .iter()
        .filter(|i| i.parent.as_ref().is_some_and(|p| p.id == parent_id))
        .collect();
    let start = after
        .and_then(|c| c.parse::<usize>().ok())
        .map_or(0, |c| c + 1);
    let end = (start + first).min(all.len());
    let children: Vec<Issue> = all
        .get(start..end)
        .unwrap_or(&[])
        .iter()
        .map(|i| (*i).clone())
        .collect();
    ChildrenPage {
        has_next_page: end < all.len(),
        end_cursor: (!children.is_empty()).then(|| (end - 1).to_string()),
        children,
    }
}

fn read_issue(inner: &Inner, id: &str) -> Option<Issue> {
    let mut issue = inner.issues.iter().find(|i| i.id == id)?.clone();
    let mut first = inner.page_size.unwrap_or(CHILDREN_FIRST_PAGE);
    let mut after: Option<String> = None;
    loop {
        let page = children_page(inner, &issue.id, first, after.as_deref());
        after = apply_children_page(&mut issue, page);
        if after.is_none() {
            break;
        }
        first = inner
            .page_size
            .or_else(|| next_children_first(issue.children.len()))
            .unwrap_or(CHILDREN_CAP);
    }
    Some(issue)
}

fn find_id(inner: &Inner, reference: &IssueRef) -> Option<String> {
    let text = reference.as_arg();
    inner
        .issues
        .iter()
        .find(|i| i.id == text || i.identifier == text)
        .map(|i| i.id.clone())
}

impl LinearApi for FakeLinear {
    fn issues(&self, ids: Vec<IssueRef>) -> BoxFuture<'static, Result<Vec<Issue>, ApiError>> {
        let call = Call::Issues(ids.iter().map(IssueRef::as_arg).collect());
        let result = self.begin(call).map(|inner| {
            ids.iter()
                .filter_map(|r| find_id(&inner, r))
                .filter_map(|id| read_issue(&inner, &id))
                .collect()
        });
        async move { result }.boxed()
    }

    fn recent(&self, limit: usize) -> BoxFuture<'static, Result<Vec<IssueSummary>, ApiError>> {
        let result = self.begin(Call::Recent(limit)).map(|inner| {
            inner
                .issues
                .iter()
                .rev()
                .take(limit)
                .map(Issue::summary)
                .collect()
        });
        async move { result }.boxed()
    }

    fn team_states(
        &self,
        team_id: &str,
    ) -> BoxFuture<'static, Result<Vec<WorkflowState>, ApiError>> {
        let result = self
            .begin(Call::TeamStates(team_id.to_string()))
            .and_then(|inner| {
                inner
                    .teams
                    .get(team_id)
                    .map(|t| t.states.clone())
                    .ok_or_else(|| ApiError::NotFound(team_id.to_string()))
            });
        async move { result }.boxed()
    }

    fn set_state(
        &self,
        issue_id: &str,
        state_id: &str,
    ) -> BoxFuture<'static, Result<(), ApiError>> {
        let call = Call::SetState {
            issue_id: issue_id.to_string(),
            state_id: state_id.to_string(),
        };
        let result = self.begin(call).and_then(|mut inner| {
            let index = inner
                .issues
                .iter()
                .position(|i| i.id == issue_id || i.identifier == issue_id)
                .ok_or_else(|| ApiError::NotFound(issue_id.to_string()))?;
            let team_id = inner.issues[index].team_id.clone();
            let state = inner
                .teams
                .get(&team_id)
                .and_then(|t| t.states.iter().find(|s| s.id == state_id))
                .cloned()
                .ok_or_else(|| ApiError::NotFound(state_id.to_string()))?;
            inner.issues[index].state = state;
            Ok(())
        });
        async move { result }.boxed()
    }

    fn create(&self, input: NewIssue) -> BoxFuture<'static, Result<IssueSummary, ApiError>> {
        let call = Call::Create {
            id: input.id,
            team_id: input.team_id.clone(),
            parent_id: input.parent_id.clone(),
            title: input.title.clone(),
        };
        let result = self.begin(call).and_then(|mut inner| {
            let id = input.id.hyphenated().to_string();
            if inner.issues.iter().any(|i| i.id == id) {
                return Err(ApiError::Duplicate(id));
            }
            let team = inner
                .teams
                .get(&input.team_id)
                .ok_or_else(|| ApiError::NotFound(input.team_id.clone()))?;
            let state = team
                .states
                .iter()
                .find(|s| s.kind == StateKind::Backlog)
                .or_else(|| team.states.first())
                .cloned()
                .ok_or_else(|| ApiError::Server("team has no states".into()))?;
            let key = team.key.clone();
            let parent = match &input.parent_id {
                Some(parent_id) => Some(
                    inner
                        .issues
                        .iter()
                        .find(|i| &i.id == parent_id || &i.identifier == parent_id)
                        .map(Issue::summary)
                        .ok_or_else(|| ApiError::NotFound(parent_id.clone()))?,
                ),
                None => None,
            };
            let number = inner
                .issues
                .iter()
                .filter(|i| i.team_id == input.team_id)
                .filter_map(|i| i.identifier.rsplit_once('-')?.1.parse::<u32>().ok())
                .max()
                .unwrap_or(0)
                + 1;
            let parsed = Identifier::new(&key, number)
                .ok_or_else(|| ApiError::Server("bad team key".into()))?;
            let identifier = parsed.to_string();
            let issue = Issue {
                url: issue_url(&inner.workspace, &parsed),
                id,
                identifier,
                title: input.title,
                description: input.description,
                team_id: input.team_id,
                state,
                parent,
                children: Vec::new(),
                children_truncated: false,
                blocks: Vec::new(),
            };
            let summary = issue.summary();
            inner.issues.push(issue);
            Ok(summary)
        });
        async move { result }.boxed()
    }

    fn viewer(&self) -> BoxFuture<'static, Result<String, ApiError>> {
        let result = self
            .begin(Call::Viewer)
            .map(|inner| inner.workspace.clone());
        async move { result }.boxed()
    }
}
