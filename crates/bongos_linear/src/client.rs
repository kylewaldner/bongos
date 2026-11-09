//! [`HttpLinear`]: the [`LinearApi`] over the application's shared HTTP client.
//!
//! Every request is one `POST` of a GraphQL body to [`ENDPOINT`] with the token in a sensitive
//! `Authorization` header, a 15 second limit for the whole exchange and a 4 MB limit on the
//! response body, of which at most 4 MB plus one byte is ever read. Redirects are not followed, so
//! the header never goes to another host.
//!
//! Error texts are redacted by construction. What this module writes itself is a fixed string:
//! never the underlying transport error, the request, its headers or its url. The only text that
//! comes from outside is a message of Linear's own GraphQL error; it is cut short, stripped of
//! control characters and scrubbed of the token and of anything shaped like a Linear key before it
//! leaves this module. Nothing here logs the token or the header.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bongos_linear_model::graphql::{
    BATCH_BUDGET, COMPLEXITY_LIMIT, ParsedIssue, Request as GraphqlRequest, ResponseMeta,
    apply_children_page, batch_size, children_page_request, create_request, issues_requests,
    parse_children_page, parse_create, parse_issues, parse_recent, parse_set_state,
    parse_team_states, parse_viewer, recent_request, set_state_request, shrink_budget,
    team_states_request, viewer_request,
};
use bongos_linear_model::{
    ApiError, Issue, IssueRef, IssueSummary, LinearApi, NewIssue, WorkflowState,
};
use futures::future::{self, BoxFuture, Either};
use futures::{AsyncReadExt as _, FutureExt as _};
use gpui::BackgroundExecutor;
use http_client::http::{HeaderMap, HeaderValue, header};
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, Method, RedirectPolicy, Url};
use serde_json::Value;

use crate::token::{ClientFactory, Token};

/// Where requests go.
pub const ENDPOINT: &str = "https://api.linear.app/graphql";
/// A debug build sends to this loopback url instead when it is set (see [`resolve_endpoint`]).
pub const OVERRIDE_ENV: &str = "BONGOS_LINEAR_API_URL";
/// How long one exchange, from sending to the last byte of the response, may take.
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// The largest response body accepted.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// The endpoint to use: `override_url` when this is a debug build and the url is
/// `http://127.0.0.1:<port>/...`, else [`ENDPOINT`]. A release build ignores the override.
pub fn resolve_endpoint(override_url: Option<&str>, is_debug: bool) -> String {
    match override_url {
        Some(url) if is_debug && is_loopback_url(url) => url.trim().to_string(),
        _ => ENDPOINT.to_string(),
    }
}

/// [`resolve_endpoint`] with the environment and this build's profile.
pub fn endpoint() -> String {
    resolve_endpoint(
        std::env::var(OVERRIDE_ENV).ok().as_deref(),
        cfg!(debug_assertions),
    )
}

/// Whether `text` is an `http` url to `127.0.0.1` with an explicit port, no credentials, no query.
pub fn is_loopback_url(text: &str) -> bool {
    let Ok(url) = Url::parse(text.trim()) else {
        return false;
    };
    url.scheme() == "http"
        && url.host() == Some(http_client::Host::Ipv4([127, 0, 0, 1].into()))
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

/// A [`ClientFactory`] that builds [`HttpLinear`]s over `http`.
pub fn http_factory(http: Arc<dyn HttpClient>, executor: BackgroundExecutor) -> ClientFactory {
    Arc::new(move |token: &Token| {
        Arc::new(HttpLinear::new(
            http.clone(),
            executor.clone(),
            token.clone(),
            endpoint(),
        )) as Arc<dyn LinearApi>
    })
}

/// Now as Unix milliseconds.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

struct Inner {
    http: Arc<dyn HttpClient>,
    executor: BackgroundExecutor,
    endpoint: String,
    token: Token,
    /// The `Authorization` value, marked sensitive. `None` when the token cannot be a header
    /// value; then nothing is sent.
    authorization: Option<HeaderValue>,
}

/// The Linear API over HTTP. `Debug` shows the endpoint only.
#[derive(Clone)]
pub struct HttpLinear {
    inner: Arc<Inner>,
}

impl fmt::Debug for HttpLinear {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpLinear")
            .field("endpoint", &self.inner.endpoint)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl HttpLinear {
    pub fn new(
        http: Arc<dyn HttpClient>,
        executor: BackgroundExecutor,
        token: Token,
        endpoint: String,
    ) -> Self {
        let authorization = HeaderValue::from_str(token.expose()).ok().map(|mut value| {
            value.set_sensitive(true);
            value
        });
        Self {
            inner: Arc::new(Inner {
                http,
                executor,
                endpoint,
                token,
                authorization,
            }),
        }
    }
}

fn network(text: &str) -> ApiError {
    ApiError::Network(text.to_string())
}

fn server(text: &str) -> ApiError {
    ApiError::Server(text.to_string())
}

/// Longest message of Linear's own that is kept.
const MAX_MESSAGE_CHARS: usize = 200;
const KEY_PREFIXES: &[&str] = &["lin_api_", "lin_oauth_"];

/// Text from outside this module, made safe to show: cut short, free of control characters,
/// without the token and without anything shaped like a Linear key.
fn scrub(text: &str, token: &Token) -> String {
    let text: String = token
        .redact(text)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some((at, prefix)) = KEY_PREFIXES
        .iter()
        .filter_map(|prefix| rest.find(prefix).map(|at| (at, *prefix)))
        .min_by_key(|(at, _)| *at)
    {
        out.push_str(&rest[..at]);
        out.push_str("[redacted]");
        let tail = &rest[at + prefix.len()..];
        let key_len = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(tail.len());
        rest = &tail[key_len..];
    }
    out.push_str(rest);
    out.chars().take(MAX_MESSAGE_CHARS).collect()
}

fn clean(error: ApiError, token: &Token) -> ApiError {
    match error {
        ApiError::Server(text) => ApiError::Server(scrub(&text, token)),
        ApiError::Network(text) => ApiError::Network(scrub(&text, token)),
        ApiError::NotFound(text) => ApiError::NotFound(scrub(&text, token)),
        ApiError::Duplicate(text) => ApiError::Duplicate(scrub(&text, token)),
        other => other,
    }
}

/// Linear documents the reset header as Unix milliseconds; a value that is plainly seconds (before
/// the year 5138 as seconds, before 1973 as milliseconds) is scaled up.
fn normalize_reset(meta: &mut ResponseMeta) {
    if let Some(reset) = meta.requests_reset_ms
        && reset < 100_000_000_000
    {
        meta.requests_reset_ms = Some(reset * 1000);
    }
}

fn meta_of(status: u16, headers: &HeaderMap) -> ResponseMeta {
    let mut meta = ResponseMeta::from_headers(status, |name| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    });
    normalize_reset(&mut meta);
    meta
}

/// One exchange: send, read at most [`MAX_RESPONSE_BYTES`] plus one byte, decode.
async fn exchange_unbounded(
    inner: &Inner,
    body: &Value,
) -> Result<(ResponseMeta, Value), ApiError> {
    let Some(authorization) = inner.authorization.clone() else {
        return Err(ApiError::Unauthorized);
    };
    let bytes = serde_json::to_vec(body).map_err(|_| server("could not encode the request"))?;
    let request = http_client::Request::builder()
        .method(Method::POST)
        .uri(inner.endpoint.as_str())
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, authorization)
        .follow_redirects(RedirectPolicy::NoFollow)
        .body(AsyncBody::from(bytes))
        .map_err(|_| network("could not build the request"))?;
    let response = inner
        .http
        .send(request)
        .await
        .map_err(|_| network("the request failed"))?;
    let status = response.status().as_u16();
    let meta = meta_of(status, response.headers());
    let mut reader = response.into_body().take(MAX_RESPONSE_BYTES as u64 + 1);
    let mut buffer = Vec::new();
    reader
        .read_to_end(&mut buffer)
        .await
        .map_err(|_| network("could not read the response"))?;
    if buffer.len() > MAX_RESPONSE_BYTES {
        return Err(server("the response is larger than 4 MB"));
    }
    let value = match serde_json::from_slice(&buffer) {
        Ok(value) => value,
        // A failure page (a proxy's or a gateway's) is not JSON; its status says enough.
        Err(_) if !(200..300).contains(&status) => Value::Null,
        Err(_) => return Err(server("malformed Linear response")),
    };
    Ok((meta, value))
}

async fn exchange(inner: &Inner, body: &Value) -> Result<(ResponseMeta, Value), ApiError> {
    let work = exchange_unbounded(inner, body).boxed();
    match future::select(work, inner.executor.timer(TIMEOUT)).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => Err(network("timed out after 15 seconds")),
    }
}

/// Sends `request` and parses its response with `parse`.
async fn call<T>(
    inner: &Inner,
    request: GraphqlRequest,
    parse: impl FnOnce(&Value, &ResponseMeta, u64) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let (meta, body) = exchange(inner, &request.body)
        .await
        .map_err(|error| clean(error, &inner.token))?;
    parse(&body, &meta, now_ms()).map_err(|error| clean(error, &inner.token))
}

/// Reads `refs` in batches sized by the server's own complexity figure, then every ticket's
/// remaining subtickets, page by page, up to the cap.
async fn read_issues(inner: &Inner, refs: Vec<IssueRef>) -> Result<Vec<Issue>, ApiError> {
    let mut issues = Vec::new();
    let mut budget = BATCH_BUDGET;
    let mut rest = refs.as_slice();
    while !rest.is_empty() {
        let size = batch_size(budget.min(COMPLEXITY_LIMIT)).min(rest.len());
        let (batch, tail) = rest.split_at(size);
        rest = tail;
        let Some(request) = issues_requests(batch, budget).into_iter().next() else {
            continue;
        };
        let (meta, body) = exchange(inner, &request.body)
            .await
            .map_err(|error| clean(error, &inner.token))?;
        if let Some(observed) = meta.complexity {
            budget = shrink_budget(budget, request.complexity, observed);
        }
        let parsed = parse_issues(batch, &body, &meta, now_ms())
            .map_err(|error| clean(error, &inner.token))?;
        for ParsedIssue {
            mut issue,
            next_children_cursor,
        } in parsed
        {
            let mut cursor = next_children_cursor;
            while let Some(after) = cursor.take() {
                let Some(request) = children_page_request(&issue.id, issue.children.len(), &after)
                else {
                    break;
                };
                let page = call(inner, request, |body, meta, now| {
                    parse_children_page(&issue.id, body, meta, now)
                })
                .await?;
                if page.children.is_empty() {
                    break;
                }
                cursor = apply_children_page(&mut issue, page);
            }
            issues.push(issue);
        }
    }
    Ok(issues)
}

impl LinearApi for HttpLinear {
    fn issues(&self, ids: Vec<IssueRef>) -> BoxFuture<'static, Result<Vec<Issue>, ApiError>> {
        let inner = self.inner.clone();
        async move { read_issues(&inner, ids).await }.boxed()
    }

    fn recent(&self, limit: usize) -> BoxFuture<'static, Result<Vec<IssueSummary>, ApiError>> {
        let inner = self.inner.clone();
        async move { call(&inner, recent_request(limit), parse_recent).await }.boxed()
    }

    fn team_states(
        &self,
        team_id: &str,
    ) -> BoxFuture<'static, Result<Vec<WorkflowState>, ApiError>> {
        let inner = self.inner.clone();
        let team_id = team_id.to_string();
        async move {
            let request = team_states_request(&team_id);
            call(&inner, request, |body, meta, now| {
                parse_team_states(&team_id, body, meta, now)
            })
            .await
        }
        .boxed()
    }

    fn set_state(
        &self,
        issue_id: &str,
        state_id: &str,
    ) -> BoxFuture<'static, Result<(), ApiError>> {
        let inner = self.inner.clone();
        let (issue_id, state_id) = (issue_id.to_string(), state_id.to_string());
        async move {
            let request = set_state_request(&issue_id, &state_id);
            call(&inner, request, |body, meta, now| {
                parse_set_state(&issue_id, body, meta, now)
            })
            .await
        }
        .boxed()
    }

    fn create(&self, input: NewIssue) -> BoxFuture<'static, Result<IssueSummary, ApiError>> {
        let inner = self.inner.clone();
        async move {
            let request = create_request(&input);
            call(&inner, request, |body, meta, now| {
                parse_create(input.id, body, meta, now)
            })
            .await
        }
        .boxed()
    }

    fn viewer(&self) -> BoxFuture<'static, Result<String, ApiError>> {
        let inner = self.inner.clone();
        async move { call(&inner, viewer_request(), parse_viewer).await }.boxed()
    }
}
