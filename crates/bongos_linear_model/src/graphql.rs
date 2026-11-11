//! GraphQL request bodies for the Linear API and the parsing of its responses.
//!
//! Everything here is pure: bodies are `serde_json::Value`s, responses are `Value`s, and nothing
//! reads or carries a credential. The sender adds headers.
//!
//! Complexity: Linear rejects any query above 10,000 points. A scalar costs 0.1 point, an object 1
//! point, and a connection costs its `first` times the cost of one node. The estimates below are
//! kept in tenths of a point and are deliberately a little high.

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::api::{ApiError, Issue, IssueRef, IssueSummary, NewIssue, StateKind, WorkflowState};

pub const COMPLEXITY_LIMIT: u32 = 10_000;
/// The budget a batch is sized to, leaving room for the estimate being low.
pub const BATCH_BUDGET: u32 = 8_000;
/// Subtickets read with the ticket itself.
pub const CHILDREN_FIRST_PAGE: usize = 25;
/// Subtickets read per follow-up page.
pub const CHILDREN_MORE_PAGE: usize = 100;
/// No ticket's subtickets are read past this many.
pub const CHILDREN_CAP: usize = 200;
pub const RELATIONS_FIRST: usize = 10;
pub const STATES_FIRST: usize = 100;
pub const RECENT_MAX: usize = 100;

const SCALAR: u32 = 1;
const OBJECT: u32 = 10;
const STATE_COST: u32 = OBJECT + 4 * SCALAR;
const TEAM_COST: u32 = OBJECT + SCALAR;
/// id, identifier, title, url, team, state.
const SUMMARY_COST: u32 = OBJECT + 4 * SCALAR + TEAM_COST + STATE_COST;
/// A summary plus the description.
const CHILD_COST: u32 = SUMMARY_COST + SCALAR;
const PAGE_INFO_COST: u32 = OBJECT + 2 * SCALAR;

/// Estimated cost, in tenths of a point, of reading one ticket with `children_first` subtickets.
pub fn issue_cost_tenths(children_first: usize) -> u32 {
    let relations = OBJECT + RELATIONS_FIRST as u32 * (OBJECT + SCALAR + OBJECT + SCALAR);
    let children = OBJECT + PAGE_INFO_COST + children_first as u32 * CHILD_COST;
    CHILD_COST + SUMMARY_COST + relations + children
}

fn tenths_to_points(tenths: u32) -> u32 {
    tenths.div_ceil(10)
}

/// A request body and its estimated complexity in points.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub body: Value,
    pub complexity: u32,
}

const SUMMARY_FRAGMENT: &str = "fragment SummaryFields on Issue { id identifier title url team { id } state { id name type position } }";
const CHILD_FRAGMENT: &str = "fragment ChildFields on Issue { id identifier title description url team { id } state { id name type position } }";

fn issue_fragment() -> String {
    format!(
        "fragment IssueFields on Issue {{ ...ChildFields parent {{ ...SummaryFields }} \
         relations(first: {RELATIONS_FIRST}) {{ nodes {{ type relatedIssue {{ id }} }} }} \
         children(first: {CHILDREN_FIRST_PAGE}) {{ pageInfo {{ hasNextPage endCursor }} \
         nodes {{ ...ChildFields }} }} }}"
    )
}

/// How many tickets fit in one query under `budget_points`. Always at least one.
pub fn batch_size(budget_points: u32) -> usize {
    let per_issue = issue_cost_tenths(CHILDREN_FIRST_PAGE);
    ((budget_points.saturating_mul(10)) / per_issue).max(1) as usize
}

/// Shrinks a budget when the server's `X-Complexity` exceeded the estimate for the same query.
pub fn shrink_budget(budget_points: u32, estimated: u32, observed: u32) -> u32 {
    if observed <= estimated || observed == 0 {
        return budget_points;
    }
    ((budget_points as u64 * estimated as u64) / observed as u64).max(1) as u32
}

/// The `issue(id:)` reads for `refs`, split into queries sized by complexity: one query with an
/// alias `i0..iN` per ticket, each with one level of children.
pub fn issues_requests(refs: &[IssueRef], budget_points: u32) -> Vec<Request> {
    let budget = budget_points.min(COMPLEXITY_LIMIT);
    refs.chunks(batch_size(budget))
        .map(issues_request)
        .collect()
}

fn issues_request(refs: &[IssueRef]) -> Request {
    let mut declarations = Vec::new();
    let mut selections = Vec::new();
    let mut variables = Map::new();
    for (index, issue) in refs.iter().enumerate() {
        declarations.push(format!("$id{index}: String!"));
        selections.push(format!(
            "i{index}: issue(id: $id{index}) {{ ...IssueFields }}"
        ));
        variables.insert(format!("id{index}"), Value::String(issue.as_arg()));
    }
    let query = format!(
        "query Issues({}) {{ {} }} {} {} {}",
        declarations.join(", "),
        selections.join(" "),
        issue_fragment(),
        CHILD_FRAGMENT,
        SUMMARY_FRAGMENT
    );
    Request {
        body: json!({ "query": query, "variables": Value::Object(variables) }),
        complexity: tenths_to_points(refs.len() as u32 * issue_cost_tenths(CHILDREN_FIRST_PAGE)),
    }
}

/// The page size for the next page of children when `have` have been read, or `None` at the cap.
pub fn next_children_first(have: usize) -> Option<usize> {
    (have < CHILDREN_CAP).then(|| CHILDREN_MORE_PAGE.min(CHILDREN_CAP - have))
}

/// `issue(id){ children(first: N, after: cursor) }`: the page after `cursor`, given `have`
/// subtickets already read. `None` once the cap is reached.
pub fn children_page_request(issue_id: &str, have: usize, after: &str) -> Option<Request> {
    let first = next_children_first(have)?;
    let query = format!(
        "query ChildrenPage($id: String!, $after: String) {{ issue(id: $id) {{ \
         children(first: {first}, after: $after) {{ pageInfo {{ hasNextPage endCursor }} \
         nodes {{ ...ChildFields }} }} }} }} {CHILD_FRAGMENT}"
    );
    let tenths = OBJECT + OBJECT + PAGE_INFO_COST + first as u32 * CHILD_COST;
    Some(Request {
        body: json!({ "query": query, "variables": { "id": issue_id, "after": after } }),
        complexity: tenths_to_points(tenths),
    })
}

/// Tickets assigned to the viewer, most recently updated first.
pub fn recent_request(limit: usize) -> Request {
    let first = limit.clamp(1, RECENT_MAX);
    let query = format!(
        "query Recent {{ viewer {{ assignedIssues(first: {first}, orderBy: updatedAt) {{ \
         nodes {{ ...SummaryFields }} }} }} }} {SUMMARY_FRAGMENT}"
    );
    Request {
        body: json!({ "query": query }),
        complexity: tenths_to_points(2 * OBJECT + OBJECT + first as u32 * SUMMARY_COST),
    }
}

pub fn team_states_request(team_id: &str) -> Request {
    let query = format!(
        "query TeamStates($id: String!) {{ team(id: $id) {{ states(first: {STATES_FIRST}) {{ \
         nodes {{ id name type position }} }} }} }}"
    );
    Request {
        body: json!({ "query": query, "variables": { "id": team_id } }),
        complexity: tenths_to_points(2 * OBJECT + OBJECT + STATES_FIRST as u32 * STATE_COST),
    }
}

pub fn set_state_request(issue_id: &str, state_id: &str) -> Request {
    Request {
        body: json!({
            "query": "mutation SetState($id: String!, $stateId: String!) { \
                      issueUpdate(id: $id, input: { stateId: $stateId }) { success } }",
            "variables": { "id": issue_id, "stateId": state_id },
        }),
        complexity: 2,
    }
}

pub fn create_request(input: &NewIssue) -> Request {
    let mut fields = Map::new();
    fields.insert("id".into(), json!(input.id.hyphenated().to_string()));
    fields.insert("teamId".into(), json!(input.team_id));
    if let Some(parent_id) = &input.parent_id {
        fields.insert("parentId".into(), json!(parent_id));
    }
    fields.insert("title".into(), json!(input.title));
    if !input.description.is_empty() {
        fields.insert("description".into(), json!(input.description));
    }
    let query = format!(
        "mutation CreateIssue($input: IssueCreateInput!) {{ issueCreate(input: $input) {{ \
         success issue {{ ...SummaryFields }} }} }} {SUMMARY_FRAGMENT}"
    );
    Request {
        body: json!({ "query": query, "variables": { "input": Value::Object(fields) } }),
        complexity: tenths_to_points(OBJECT + OBJECT + SUMMARY_COST + SCALAR),
    }
}

pub fn viewer_request() -> Request {
    Request {
        body: json!({ "query": "query Viewer { viewer { organization { name } } }" }),
        complexity: 3,
    }
}

// ---------------------------------------------------------------------------------------------
// Responses

/// What the sender read from the HTTP response besides the body. Names are Linear's headers
/// `Retry-After`, `X-RateLimit-Requests-Reset` and `X-Complexity`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponseMeta {
    pub status: u16,
    pub retry_after_secs: Option<u64>,
    /// `X-RateLimit-Requests-Reset`: Unix time in milliseconds, per Linear's documentation.
    pub requests_reset_ms: Option<u64>,
    /// `X-Complexity`: the server's cost for the query.
    pub complexity: Option<u32>,
    /// `X-RateLimit-Requests-Remaining`: requests left in the current window.
    pub requests_remaining: Option<u32>,
}

/// Automatic refreshes pause when fewer requests than this remain in the window.
pub const LOW_REQUESTS: u32 = 20;

impl ResponseMeta {
    /// Reads the relevant headers through `header`, which looks one up by lowercase name.
    pub fn from_headers(status: u16, header: impl Fn(&str) -> Option<String>) -> Self {
        let number = |name: &str| header(name).and_then(|v| v.trim().parse::<u64>().ok());
        Self {
            status,
            retry_after_secs: number("retry-after"),
            requests_reset_ms: number("x-ratelimit-requests-reset"),
            complexity: number("x-complexity").and_then(|v| u32::try_from(v).ok()),
            requests_remaining: number("x-ratelimit-requests-remaining")
                .and_then(|v| u32::try_from(v).ok()),
        }
    }

    /// Whether the window is nearly used up ([`LOW_REQUESTS`]).
    pub fn requests_low(&self) -> bool {
        self.requests_remaining.is_some_and(|n| n < LOW_REQUESTS)
    }

    /// When the limit lifts, as Unix milliseconds: the later of the reset header and
    /// `now + Retry-After`.
    pub fn reset_at(&self, now_ms: u64) -> Option<u64> {
        let from_retry = self.retry_after_secs.map(|s| now_ms + s * 1000);
        match (self.requests_reset_ms, from_retry) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Maps a response to an error, or `Ok(())` when it carries none. `context` names the thing the
/// request was about (a ticket id), for `NotFound` and `Duplicate`.
pub fn classify(
    meta: &ResponseMeta,
    body: &Value,
    context: &str,
    now_ms: u64,
) -> Result<(), ApiError> {
    if meta.status == 429 {
        return Err(ApiError::RateLimited {
            reset_at: meta.reset_at(now_ms),
        });
    }
    if matches!(meta.status, 401 | 403) {
        return Err(ApiError::Unauthorized);
    }
    if let Some(error) = first_graphql_error(body, |_| false) {
        return Err(error_from_graphql(error, meta, context, now_ms));
    }
    if !(200..300).contains(&meta.status) {
        return Err(ApiError::Server(format!("HTTP {}", meta.status)));
    }
    Ok(())
}

fn first_graphql_error(body: &Value, skip: impl Fn(&Value) -> bool) -> Option<&Value> {
    body.get("errors")?
        .as_array()?
        .iter()
        .find(|error| !skip(error))
}

fn error_code(error: &Value) -> &str {
    error
        .pointer("/extensions/code")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn error_message(error: &Value) -> &str {
    error.get("message").and_then(Value::as_str).unwrap_or("")
}

fn is_not_found(error: &Value) -> bool {
    let message = error_message(error).to_ascii_lowercase();
    error_code(error).eq_ignore_ascii_case("ENTITY_NOT_FOUND") || message.contains("not found")
}

fn error_from_graphql(error: &Value, meta: &ResponseMeta, context: &str, now_ms: u64) -> ApiError {
    let code = error_code(error).to_ascii_uppercase();
    let message = error_message(error);
    let lower = message.to_ascii_lowercase();
    match code.as_str() {
        "RATELIMITED" => ApiError::RateLimited {
            reset_at: meta.reset_at(now_ms),
        },
        "AUTHENTICATION_ERROR" | "FORBIDDEN" => ApiError::Unauthorized,
        _ if is_not_found(error) => ApiError::NotFound(context.to_string()),
        "DUPLICATE" => ApiError::Duplicate(context.to_string()),
        _ if lower.contains("duplicate")
            || lower.contains("already exists")
            || lower.contains("must be unique") =>
        {
            ApiError::Duplicate(context.to_string())
        }
        _ => ApiError::Server(message.to_string()),
    }
}

fn malformed(what: &str) -> ApiError {
    ApiError::Server(format!("malformed Linear response: {what}"))
}

fn text(value: &Value, key: &str) -> Result<String, ApiError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| malformed(&format!("missing {key}")))
}

fn parse_state(value: &Value) -> Result<WorkflowState, ApiError> {
    Ok(WorkflowState {
        id: text(value, "id")?,
        name: text(value, "name")?,
        kind: StateKind::parse(&text(value, "type")?),
        position: value
            .get("position")
            .and_then(Value::as_f64)
            .ok_or_else(|| malformed("missing position"))?,
    })
}

fn parse_summary(value: &Value) -> Result<IssueSummary, ApiError> {
    Ok(IssueSummary {
        id: text(value, "id")?,
        identifier: text(value, "identifier")?,
        title: text(value, "title")?,
        url: text(value, "url")?,
        team_id: value
            .pointer("/team/id")
            .and_then(Value::as_str)
            .ok_or_else(|| malformed("missing team"))?
            .to_string(),
        state: parse_state(
            value
                .get("state")
                .ok_or_else(|| malformed("missing state"))?,
        )?,
    })
}

/// A subticket node (no parent, no further children, no relations).
fn parse_child(value: &Value) -> Result<Issue, ApiError> {
    let summary = parse_summary(value)?;
    Ok(Issue {
        id: summary.id,
        identifier: summary.identifier,
        title: summary.title,
        description: value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        url: summary.url,
        team_id: summary.team_id,
        state: summary.state,
        parent: None,
        children: Vec::new(),
        children_truncated: false,
        blocks: Vec::new(),
    })
}

/// One page of subtickets.
#[derive(Clone, Debug, PartialEq)]
pub struct ChildrenPage {
    pub children: Vec<Issue>,
    pub has_next_page: bool,
    pub end_cursor: Option<String>,
}

fn parse_connection(connection: &Value) -> Result<ChildrenPage, ApiError> {
    let nodes = connection
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("missing children nodes"))?;
    let page_info = connection.get("pageInfo");
    Ok(ChildrenPage {
        children: nodes.iter().map(parse_child).collect::<Result<_, _>>()?,
        has_next_page: page_info
            .and_then(|p| p.get("hasNextPage"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        end_cursor: page_info
            .and_then(|p| p.get("endCursor"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// A ticket as read by `issues_requests`, plus where to continue reading its subtickets.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedIssue {
    pub issue: Issue,
    /// The `after:` cursor for the next page of subtickets; `None` when all were read or the
    /// cap was hit.
    pub next_children_cursor: Option<String>,
}

fn parse_issue(value: &Value) -> Result<ParsedIssue, ApiError> {
    let mut issue = parse_child(value)?;
    issue.parent = match value.get("parent") {
        Some(parent) if !parent.is_null() => Some(parse_summary(parent)?),
        _ => None,
    };
    if let Some(nodes) = value.pointer("/relations/nodes").and_then(Value::as_array) {
        issue.blocks = nodes
            .iter()
            .filter(|node| node.get("type").and_then(Value::as_str) == Some("blocks"))
            .filter_map(|node| node.pointer("/relatedIssue/id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
    }
    let mut next_children_cursor = None;
    if let Some(connection) = value.get("children").filter(|c| !c.is_null()) {
        let page = parse_connection(connection)?;
        next_children_cursor = apply_children_page(&mut issue, page);
    }
    Ok(ParsedIssue {
        issue,
        next_children_cursor,
    })
}

/// Adds a page of subtickets to `issue`, never past `CHILDREN_CAP`, setting `children_truncated`
/// when more exist beyond the cap. Returns the cursor to continue with, if any.
pub fn apply_children_page(issue: &mut Issue, page: ChildrenPage) -> Option<String> {
    let room = CHILDREN_CAP.saturating_sub(issue.children.len());
    if page.children.len() > room {
        issue.children_truncated = true;
    }
    issue.children.extend(page.children.into_iter().take(room));
    if !page.has_next_page {
        return None;
    }
    if issue.children.len() >= CHILDREN_CAP {
        issue.children_truncated = true;
        return None;
    }
    page.end_cursor
}

/// Parses the response to one `issues_requests` query for `refs`. References Linear does not know
/// (a null alias with an "entity not found" error) are left out; any other error fails the batch.
pub fn parse_issues(
    refs: &[IssueRef],
    response: &Value,
    meta: &ResponseMeta,
    now_ms: u64,
) -> Result<Vec<ParsedIssue>, ApiError> {
    let tolerated = |error: &Value| {
        is_not_found(error)
            && error
                .pointer("/path/0")
                .and_then(Value::as_str)
                .is_some_and(|alias| {
                    response
                        .pointer(&format!("/data/{alias}"))
                        .is_some_and(Value::is_null)
                })
    };
    if meta.status == 429 || matches!(meta.status, 401 | 403) {
        classify(meta, response, "", now_ms)?;
    }
    if let Some(error) = first_graphql_error(response, tolerated) {
        return Err(error_from_graphql(error, meta, "", now_ms));
    }
    if !(200..300).contains(&meta.status) {
        return Err(ApiError::Server(format!("HTTP {}", meta.status)));
    }
    let data = response
        .get("data")
        .ok_or_else(|| malformed("missing data"))?;
    let mut parsed = Vec::new();
    for index in 0..refs.len() {
        match data.get(format!("i{index}")) {
            Some(value) if !value.is_null() => parsed.push(parse_issue(value)?),
            _ => {}
        }
    }
    Ok(parsed)
}

/// Parses the response to a `children_page_request` for `issue_id`.
pub fn parse_children_page(
    issue_id: &str,
    response: &Value,
    meta: &ResponseMeta,
    now_ms: u64,
) -> Result<ChildrenPage, ApiError> {
    classify(meta, response, issue_id, now_ms)?;
    let connection = response
        .pointer("/data/issue/children")
        .filter(|c| !c.is_null())
        .ok_or_else(|| ApiError::NotFound(issue_id.to_string()))?;
    parse_connection(connection)
}

pub fn parse_recent(
    response: &Value,
    meta: &ResponseMeta,
    now_ms: u64,
) -> Result<Vec<IssueSummary>, ApiError> {
    classify(meta, response, "viewer", now_ms)?;
    response
        .pointer("/data/viewer/assignedIssues/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| malformed("missing assignedIssues"))?
        .iter()
        .map(parse_summary)
        .collect()
}

pub fn parse_team_states(
    team_id: &str,
    response: &Value,
    meta: &ResponseMeta,
    now_ms: u64,
) -> Result<Vec<WorkflowState>, ApiError> {
    classify(meta, response, team_id, now_ms)?;
    let nodes = response
        .pointer("/data/team/states/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::NotFound(team_id.to_string()))?;
    nodes.iter().map(parse_state).collect()
}

pub fn parse_set_state(
    issue_id: &str,
    response: &Value,
    meta: &ResponseMeta,
    now_ms: u64,
) -> Result<(), ApiError> {
    classify(meta, response, issue_id, now_ms)?;
    match response
        .pointer("/data/issueUpdate/success")
        .and_then(Value::as_bool)
    {
        Some(true) => Ok(()),
        _ => Err(ApiError::Server("issueUpdate did not succeed".into())),
    }
}

pub fn parse_create(
    id: Uuid,
    response: &Value,
    meta: &ResponseMeta,
    now_ms: u64,
) -> Result<IssueSummary, ApiError> {
    classify(meta, response, &id.hyphenated().to_string(), now_ms)?;
    let create = response
        .pointer("/data/issueCreate")
        .ok_or_else(|| malformed("missing issueCreate"))?;
    if create.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(ApiError::Server("issueCreate did not succeed".into()));
    }
    parse_summary(
        create
            .get("issue")
            .ok_or_else(|| malformed("missing issue"))?,
    )
}

/// The workspace (organization) name.
pub fn parse_viewer(
    response: &Value,
    meta: &ResponseMeta,
    now_ms: u64,
) -> Result<String, ApiError> {
    classify(meta, response, "viewer", now_ms)?;
    response
        .pointer("/data/viewer/organization/name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| malformed("missing organization name"))
}
